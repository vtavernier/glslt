use std::sync::Arc;

use glsl_lang::{ast::*, visitor::*};

use indexmap::IndexMap;

use super::instantiate::InstantiateTemplate;
use super::{FnHandle, FnRef, GlobalScope, ParsedDeclaration, TransformUnit};

use crate::{glsl_ext::FunIdentifierExt, Error, Result, TransformConfig};

mod dependency_dag;
use dependency_dag::*;

/// Minimizing GLSLT template instantiation context
#[derive(Default, Debug, Clone)]
pub struct MinUnit {
    /// Template definition global scope
    global_scope: GlobalScope,
    /// External declaration repository
    external_declarations: IndexMap<ExternalIdentifier, Arc<ExternalDeclaration>>,
    /// Dependency graph, built as declarations are added to this unit
    dag: DependencyDag,
    /// Static contents that can be included before other declarations (such as #version, precision
    /// qualifiers, etc.)
    static_declarations: Vec<Arc<ExternalDeclaration>>,
}

impl MinUnit {
    /// Create a new minifying transform unit
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new minifying transform unit using pre-defined templates
    ///
    /// # Parameters
    ///
    /// * `global_scope`: context to pull pre-defined templates from
    pub fn with_context(global_scope: GlobalScope) -> Self {
        Self {
            global_scope,
            ..Default::default()
        }
    }

    /// Create a new transform unit with the given configuration
    ///
    /// # Parameters
    ///
    /// * `config`: transform configuration
    pub fn with_config(config: TransformConfig) -> Self {
        Self {
            global_scope: GlobalScope::with_config(config),
            ..Default::default()
        }
    }

    /// Obtain an iterator to the functions and templates defined in the current unit
    pub fn iter_functions(&self) -> impl Iterator<Item = Node<FnRef>> {
        self.external_declarations
            .values()
            .filter_map(|ed| match ed.content {
                ExternalDeclarationData::FunctionDefinition(ref fd) => Some(Node::new(
                    FnRef {
                        prototype: &fd.prototype,
                        statement: &fd.statement,
                    },
                    ed.span,
                )),
                _ => None,
            })
            .chain(
                self.global_scope
                    .declared_templates()
                    .values()
                    .map(|dt| dt.fn_ref()),
            )
    }

    /// Obtain an iterator to all the top-level declarations in the current unit
    pub fn iter_declarations(&self) -> impl Iterator<Item = &ExternalDeclaration> {
        self.external_declarations.values().map(|decl| &**decl)
    }

    /// Transform this unit into a GLSL syntax tree
    ///
    /// Only the functions included in `wanted` and their transitive dependencies will be included
    /// in the output.
    ///
    /// # Parameters
    ///
    /// * `wanted`: list of function names to include in the result
    pub fn into_translation_unit<'a>(
        self,
        wanted: impl std::iter::Iterator<Item = &'a str>,
    ) -> Result<TranslationUnit> {
        // Resulting declarations
        let mut external_declarations = self.static_declarations;

        // TODO: Maybe wanted can include other declarations than functions?
        let wanted: Vec<_> = wanted
            .map(|id| ExternalIdentifier::FunctionDefinition(id.into()))
            .collect();

        // Extract declarations
        let mut stored_decls = self.external_declarations;

        // Add dependencies from wanted into the result
        // TODO: filter_map just ignores missing dependencies, we should at least warn the user
        external_declarations.extend(
            self.dag
                .into_dependencies(&wanted)
                .into_iter()
                .filter_map(|id| stored_decls.remove(&id)),
        );

        Ok(TranslationUnit(
            external_declarations
                .into_iter()
                .map(|arc| Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone()))
                .collect(),
        ))
    }

    fn extend_dag(&mut self, tu: &impl Host) {
        struct VisitorData<'e> {
            current_scope_name: Option<usize>,
            this: &'e mut MinUnit,
        }

        impl Visitor for VisitorData<'_> {
            fn visit_external_declaration(&mut self, _node: &ExternalDeclaration) -> Visit {
                self.current_scope_name = None;

                Visit::Children
            }

            fn visit_preprocessor_define(&mut self, def: &PreprocessorDefine) -> Visit {
                // Declare the current scope name
                let csn = self
                    .this
                    .dag
                    .declare_symbol(ExternalId::Declaration(match &**def {
                        PreprocessorDefineData::ObjectLike { ident, .. } => &ident.0,
                        PreprocessorDefineData::FunctionLike { ident, .. } => &ident.0,
                    }));

                self.current_scope_name = Some(csn);

                // Check the list of identifiers that should not be considered external
                // dependencies
                let mut scope_identifiers = std::collections::HashSet::new();
                if let PreprocessorDefineData::FunctionLike { args, .. } = &**def {
                    scope_identifiers.extend(args.iter().map(|a| a.0.as_str()));
                }

                // The value is not parsed by the glsl crate, so we need to extract identifiers
                // ourselves
                for ident in extract_idents(&match &**def {
                    PreprocessorDefineData::ObjectLike { value, .. } => value,
                    PreprocessorDefineData::FunctionLike { value, .. } => value,
                }) {
                    let symbol = self.this.dag.declare_symbol(ExternalId::Declaration(ident));
                    self.this.dag.add_dep(csn, symbol);
                }

                Visit::Parent
            }

            fn visit_struct_specifier(&mut self, node: &StructSpecifier) -> Visit {
                if self.current_scope_name.is_none() {
                    if let Some(name) = &node.name {
                        self.current_scope_name = Some(
                            self.this
                                .dag
                                .declare_symbol(ExternalId::Declaration(name.0.as_str())),
                        );
                    }
                }

                Visit::Children
            }

            fn visit_function_definition(&mut self, node: &FunctionDefinition) -> Visit {
                self.current_scope_name = Some(self.this.dag.declare_symbol(
                    ExternalId::FunctionDefinition(node.prototype.name.0.as_str()),
                ));

                Visit::Children
            }

            fn visit_type_name(&mut self, node: &TypeName) -> Visit {
                if let Some(csn) = self.current_scope_name {
                    let this = self
                        .this
                        .dag
                        .declare_symbol(ExternalId::Declaration(node.0.as_str()));

                    if this != csn {
                        self.this.dag.add_dep(csn, this);
                    }
                }

                Visit::Children
            }

            fn visit_fun_identifier(&mut self, node: &FunIdentifier) -> Visit {
                if let Some(ident) = node.as_ident_or_type_name() {
                    if let Some(csn) = self.current_scope_name {
                        let this = self
                            .this
                            .dag
                            .declare_symbol(ExternalId::FunctionDefinition(ident.as_str()));
                        self.this.dag.add_dep(csn, this);
                    }
                }

                Visit::Children
            }

            fn visit_identifier(&mut self, node: &Identifier) -> Visit {
                if self
                    .this
                    .external_declarations
                    .contains_key(&ExternalIdentifier::Declaration(node.0.clone()))
                {
                    if let Some(csn) = self.current_scope_name {
                        // TODO: Declaration?
                        let this = self
                            .this
                            .dag
                            .declare_symbol(ExternalId::Declaration(node.0.as_str()));
                        self.this.dag.add_dep(csn, this);
                    }
                }

                Visit::Children
            }
        }

        let mut visitor = VisitorData {
            current_scope_name: None,
            this: self,
        };

        tu.visit(&mut visitor);
    }
}

impl TransformUnit for MinUnit {
    fn global_scope(&self) -> &GlobalScope {
        &self.global_scope
    }

    fn global_scope_mut(&mut self) -> &mut GlobalScope {
        &mut self.global_scope
    }

    fn push_function_declaration(&mut self, def: FunctionDefinition) {
        // Register the function as a known function
        self.global_scope
            .known_functions_mut()
            .insert(def.prototype.name.0.clone(), def.prototype.clone());

        // Register it in the dependency graph
        self.extend_dag(&def);

        // Add the definition to the declarations
        // TODO: Don't clone def.span?
        let span = def.span;
        let id = ExternalIdentifier::FunctionDefinition(def.prototype.name.0.clone());
        self.external_declarations.insert(
            id,
            Arc::new(ExternalDeclaration::new(
                ExternalDeclarationData::FunctionDefinition(def),
                span,
            )),
        );
    }

    fn parse_external_declaration(
        &mut self,
        extdecl: ExternalDeclaration,
    ) -> Result<Option<FnHandle>> {
        let unparsed;

        match self.global_scope.parse_external_declaration(extdecl)? {
            ParsedDeclaration::ConsumedAsType => {
                return Ok(None);
            }
            ParsedDeclaration::ConsumedAsTemplate(r) => {
                return Ok(Some(r.into()));
            }
            ParsedDeclaration::Unparsed(extdecl) => {
                unparsed = extdecl;
            }
        }

        let extdecl = Arc::try_unwrap(unparsed).unwrap();
        match extdecl.content {
            ExternalDeclarationData::FunctionDefinition(def) => {
                // No template parameter, it's a "regular" function so it has to be
                // processed to instantiate parameters
                let decls =
                    InstantiateTemplate::new(None).instantiate(&mut self.global_scope, def)?;

                for d in decls {
                    self.push_function_declaration(d);
                }

                let f = self.external_declarations.last().unwrap();
                match &f.1.content {
                    ExternalDeclarationData::FunctionDefinition(def) => {
                        return Ok(Some(
                            Node::new(
                                FnRef {
                                    prototype: &def.prototype,
                                    statement: &def.statement,
                                },
                                extdecl.span,
                            )
                            .into(),
                        ));
                    }
                    _ => unreachable!(),
                }
            }
            other => match other {
                ExternalDeclarationData::FunctionDefinition(_) => {}
                ExternalDeclarationData::Preprocessor(ref pp) => match &pp.content {
                    PreprocessorData::Define(PreprocessorDefine {
                        content: PreprocessorDefineData::ObjectLike { ident, .. },
                        ..
                    }) => {
                        self.extend_dag(pp);

                        self.external_declarations.insert(
                            ExternalIdentifier::Declaration(ident.0.clone()),
                            Arc::new(Node::new(other, extdecl.span)),
                        );
                    }
                    PreprocessorData::Define(PreprocessorDefine {
                        content: PreprocessorDefineData::FunctionLike { ident, .. },
                        ..
                    }) => {
                        self.extend_dag(pp);

                        self.external_declarations.insert(
                            ExternalIdentifier::FunctionDefinition(ident.0.clone()),
                            Arc::new(Node::new(other, extdecl.span)),
                        );
                    }
                    PreprocessorData::Version(_) | PreprocessorData::Extension(_) => {
                        self.static_declarations
                            .push(Arc::new(Node::new(other, extdecl.span)));
                    }
                    rest => {
                        return Err(Error::UnsupportedPreprocessor(Preprocessor::new(
                            rest.clone(),
                            pp.span,
                        )))
                    }
                },
                ExternalDeclarationData::Declaration(ref decl) => match &decl.content {
                    DeclarationData::FunctionPrototype(_) => {
                        unreachable!("prototype already consumed by template engine")
                    }
                    DeclarationData::InitDeclaratorList(idl) => {
                        // TODO: Handle variable declarations at top-level using
                        // InitDeclaratorList. For now, this only handles struct declarations.
                        if let TypeSpecifierNonArrayData::Struct(ss) = &*idl.head.ty.ty.ty {
                            // It's a struct declaration
                            if let Some(tn) = &ss.name {
                                // Dependency key
                                let key = ExternalIdentifier::Declaration(tn.0.clone());
                                // Node for dependency walking and storage
                                let node = Node::new(other, extdecl.span);

                                // Parse type name dependencies in the struct specification
                                self.extend_dag(&node);

                                self.external_declarations.insert(key, Arc::new(node));
                            } else {
                                return Err(Error::UnsupportedIdl(idl.clone()));
                            }
                        } else {
                            // It's a variable declaration, maybe?
                            if let Some(name) = &idl.head.name {
                                self.external_declarations.insert(
                                    ExternalIdentifier::Declaration(name.0.clone()),
                                    Arc::new(Node::new(other, extdecl.span)),
                                );
                            } else {
                                return Err(Error::UnsupportedIdl(idl.clone()));
                            }
                        }
                    }
                    DeclarationData::Precision(_, _) | DeclarationData::Block(_) => {
                        // TODO: How to handle Declaration::Block?
                        self.static_declarations
                            .push(Arc::new(Node::new(other, extdecl.span)));
                    }
                },
            },
        }

        Ok(None)
    }
}
