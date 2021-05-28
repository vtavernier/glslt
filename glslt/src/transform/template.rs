//! Definitions of template function abstractrepresentations

use glsl_lang::ast::*;

use indexmap::IndexMap;

use super::{instantiate::InstantiateTemplate, FnRef, LocalScope, Scope};

use crate::{Error, Result};

/// Function parameter of a template
#[derive(Debug, Clone)]
pub struct TemplateParameter {
    /// Name of the function pointer type
    pub typename: SmolStr,
    /// Template variable name
    pub symbol: Option<SmolStr>,
    /// Original parameter index
    pub index: usize,
}

/// Definition of a template function
#[derive(Debug, Clone)]
pub struct TemplateDefinition {
    /// AST for the partially instantiated template definition.
    ///
    /// This has to be cloned and visited to replace the template parameters.
    ast: FunctionDefinition,
    /// List of template parameters
    parameters: Vec<TemplateParameter>,
    /// Original prototype
    raw_prototype: FunctionPrototype,
}

fn expr_vec_to_id(exprs: &[(Expr, &str)]) -> String {
    let mut sbuf = String::new();

    // Transpile all expressions into the string buffer
    for expr in exprs {
        sbuf.push_str(expr.1);

        glsl_lang::transpiler::glsl::show_expr(
            &mut sbuf,
            &expr.0,
            &mut glsl_lang::transpiler::glsl::FormattingState::default(),
        )
        .unwrap();
    }

    // Compute it's SHA-1
    use sha1::{Digest, Sha1};
    format!("{:x}", Sha1::digest(&sbuf.as_bytes()))[0..6].to_string()
}

impl TemplateDefinition {
    /// Get the AST of this template definition
    pub fn ast(&self) -> &FunctionDefinition {
        &self.ast
    }

    /// Get the list of parameters of this template
    pub fn parameters(&self) -> &[TemplateParameter] {
        &self.parameters[..]
    }

    /// Get the FnRef definition of this template
    pub fn fn_ref(&self) -> Node<FnRef> {
        Node::new(
            FnRef {
                prototype: &self.raw_prototype,
                statement: &self.ast.statement,
            },
            self.ast.span,
        )
    }

    /// Generate a unique ID for the given template invocation
    ///
    /// # Parameters
    ///
    /// * `args`: list of template parameter values used in the invocation
    /// * `config`: scope in which this identifier should be generated
    pub fn generate_id(&self, args: &[(Expr, &str)], scope: &dyn Scope) -> SmolStr {
        let args_id = expr_vec_to_id(&args);
        let base = scope.config().prefix.clone() + self.ast.prototype.name.0.as_str();
        SmolStr::from([base.as_str(), &args_id].join("_"))
    }

    /// Instantiate this template definition into a GLSL function
    ///
    /// # Parameters
    ///
    /// * `scope`: local scope this template is being instantiated from
    /// * `outer_instantiator`: outer instantiation context
    /// * `config`: transformation config
    pub fn instantiate(
        &self,
        scope: &mut LocalScope,
        outer_instantiator: &InstantiateTemplate,
    ) -> crate::Result<Vec<FunctionDefinition>> {
        // Clone the AST
        let ast = self.ast.clone();

        // We're entering a new function, thus we need a new context
        let mut res =
            InstantiateTemplate::new(Some(&outer_instantiator)).instantiate(scope, ast)?;

        // The last function is the current instantiated one, the ones before are dependencies
        // TODO: Make this more robust
        let ast = res.last_mut().unwrap();

        // Change the name
        debug!("renaming {} to {}", ast.prototype.name.0, scope.name());
        ast.prototype.name.0 = scope.name().into();

        // Add the captured parameters to the signature
        for ep in scope.captured_parameters() {
            // TODO: Span information?
            ast.prototype.parameters.push(
                FunctionParameterDeclarationData::Named(
                    None,
                    FunctionParameterDeclaratorData {
                        ty: ep.decl_type.clone(),
                        ident: ArrayedIdentifierData {
                            ident: IdentifierData(ep.gen_id.clone()).into(),
                            array_spec: ep.array.clone(),
                        }
                        .into(),
                    }
                    .into(),
                )
                .into(),
            );
        }

        Ok(res)
    }

    /// Extract the template parameters from the full set of call parameters
    ///
    /// # Parameters
    ///
    /// * `args`: list of all function call arguments
    ///
    /// # Returns
    ///
    /// List of expressions to be used in the template call. `args` will contain regular arguments
    /// to the GLSL function (which do not require a template instantiation).
    pub fn extract_template_parameters(&self, args: &mut Vec<Expr>) -> Result<Vec<(Expr, &str)>> {
        let mut idx = 0;
        let mut it = self.parameters.iter();
        let mut current = it.next();

        let (res, other): (Vec<_>, Vec<_>) = args.drain(0..args.len()).partition(|_| {
            let res = if let Some(c) = current {
                if c.index <= idx {
                    current = it.next();
                    true
                } else {
                    false
                }
            } else {
                false
            };

            idx += 1;
            res
        });

        // Put regular args back into the function call
        args.extend(other.into_iter());

        // Return template args with their values and type names
        Ok(res
            .into_iter()
            .enumerate()
            .map(|(id, arg)| (arg, self.parameters[id].typename.as_str()))
            .collect())
    }
}

/// Result of parsing a function definition
pub enum TryTemplate {
    /// GLSLT template function
    Template(TemplateDefinition),
    /// GLSL function
    Function(FunctionDefinition),
}

/// Try parsing a function definition as a template
///
/// # Parameters
///
/// * `def`: function definition to parse
/// * `declared_pointer_types`: map of known function pointer types
///
/// # Returns
///
/// A [TryTemplate] structure which either represents a regular function when no template
/// parameters are present, or a template function if it will have to be instantiated into an
/// actual GLSL function.
///
/// # Errors
///
/// See [crate::Error] for potential template declaration errors.
pub fn parse_definition_as_template(
    def: FunctionDefinition,
    declared_pointer_types: &IndexMap<SmolStr, FunctionPrototype>,
) -> Result<TryTemplate> {
    let mut parameters = Vec::new();
    let mut non_template_parameters = Vec::new();
    let span = def.span;
    let mut def = def.into_inner();
    let raw_prototype = def.prototype.clone();

    let len = def.prototype.parameters.len();
    let name: SmolStr = def.prototype.name.as_str().into();

    for (arg_id, parameter) in def.prototype.parameters.drain(0..len).enumerate() {
        let (n, t) = match &*parameter {
            FunctionParameterDeclarationData::Named(_, d) => (Some(d.ident.ident.0.clone()), &d.ty),
            FunctionParameterDeclarationData::Unnamed(_, t) => (None, t),
        };

        if let TypeSpecifierNonArrayData::TypeName(tn) = &*t.ty {
            if declared_pointer_types.contains_key(&tn.0) {
                if t.array_specifier.is_some() {
                    return Err(Error::ArrayedTemplateParameter {
                        name,
                        index: arg_id,
                    });
                } else {
                    parameters.push(TemplateParameter {
                        typename: tn.0.clone(),
                        symbol: n,
                        index: arg_id,
                    });

                    continue;
                }
            }
        }

        non_template_parameters.push(parameter);
    }

    def.prototype
        .parameters
        .extend(non_template_parameters.into_iter());

    let def = Node::new(def, span);

    if parameters.is_empty() {
        Ok(TryTemplate::Function(def))
    } else {
        Ok(TryTemplate::Template(TemplateDefinition {
            ast: def,
            parameters,
            raw_prototype,
        }))
    }
}
