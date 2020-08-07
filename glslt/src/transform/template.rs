//! Definitions of template function abstractrepresentations

use std::collections::HashMap;
use std::num::NonZeroUsize;

use glsl::syntax::*;
use glsl::visitor::*;

use super::LocalScope;

use crate::{Error, Result};

/// Function parameter of a template
#[derive(Debug, Clone)]
pub struct TemplateParameter {
    /// Name of the function pointer type
    pub typename: String,
    /// Template variable name
    pub symbol: Option<String>,
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
    /// Declaring span id
    span_id: Option<NonZeroUsize>,
}

fn arg_instantiate(tgt: &mut Expr, source_parameters: &[Expr], prototype: &FunctionPrototype) {
    // Declare the visitor for the substitution
    struct V<'s> {
        subs: HashMap<String, &'s Expr>,
    }

    impl Visitor for V<'_> {
        fn visit_expr(&mut self, e: &mut Expr) -> Visit {
            if let Expr::Variable(ident) = e {
                if let Some(repl) = self.subs.get(ident.0.as_str()) {
                    *e = (*repl).clone();
                }
            }

            Visit::Children
        }
    }

    // Perform substitutions
    let mut subs = HashMap::new();
    for (id, value) in source_parameters.iter().enumerate() {
        subs.insert(format!("_{}", id + 1), value);

        if let FunctionParameterDeclaration::Named(_, p) = &prototype.parameters[id] {
            subs.insert(format!("_{}", p.ident.ident.0), value);
        }
    }

    tgt.visit(&mut V { subs });
}

fn expr_vec_to_id(exprs: &[Expr]) -> String {
    let mut sbuf = String::new();

    // Transpile all expressions into the string buffer
    for expr in exprs {
        glsl::transpiler::glsl::show_expr(
            &mut sbuf,
            expr,
            &mut glsl::transpiler::glsl::FormattingState::default(),
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

    /// Get the span id where this template was declared
    pub fn span_id(&self) -> Option<NonZeroUsize> {
        self.span_id
    }

    /// Add span information to this template
    pub fn with_span_id(self, span_id: Option<NonZeroUsize>) -> Self {
        Self { span_id, ..self }
    }

    /// Generate a unique ID for the given template invocation
    ///
    /// # Parameters
    ///
    /// * `args`: list of template parameter values used in the invocation
    pub fn generate_id(&self, args: &[Expr]) -> String {
        let args_id = expr_vec_to_id(&args);
        ["_glslt", self.ast.prototype.name.0.as_str(), &args_id].join("_")
    }

    /// Instantiate this template definition into a GLSL function
    ///
    /// # Parameters
    ///
    /// * `scope`: local scope this template is being instantiated from
    /// * `instantiator`: source visitor for the template instantiator
    /// * `unit`: host transformation unit
    pub fn instantiate(
        &self,
        scope: &LocalScope,
        instantiator: &mut super::instantiate::InstantiateTemplate,
        unit: &mut dyn super::TransformUnit,
    ) -> Node<FunctionDefinition> {
        // Clone the AST
        let mut ast = self.ast.clone();

        // Declare the visitor for the substitution
        struct V<'s> {
            subs: HashMap<&'s str, &'s Expr>,
            template_parameters: HashMap<&'s str, &'s TemplateParameter>,
            instantiator: &'s mut super::instantiate::InstantiateTemplate,
            unit: &'s mut dyn super::TransformUnit,
        }

        impl Visitor for V<'_> {
            fn visit_expr(&mut self, e: &mut Expr) -> Visit {
                if let Expr::FunCall(fun, src_args) = e {
                    // Transform arguments first
                    for arg in src_args.iter_mut() {
                        arg.visit(self);
                    }

                    // Only consider raw identifiers for function names
                    if let FunIdentifier::Identifier(ident) = fun {
                        if let Some(arg) = self.subs.get(ident.0.as_str()) {
                            // This is the name of a function to be templated

                            // If the substitution is a function name, just replace it and pass
                            // argument as-is.
                            //
                            // Else, replace the entire function call with the templated
                            // expression
                            match arg {
                                Expr::Variable(arg_ident)
                                    if self
                                        .unit
                                        .known_functions()
                                        .contains(arg_ident.0.as_str()) =>
                                {
                                    debug!("raw function name: {:?}", arg_ident);
                                    ident.0 = arg_ident.0.clone();
                                }
                                other => {
                                    debug!("lambda expression: {:?}", other);
                                    let mut res = (*other).clone();
                                    arg_instantiate(
                                        &mut res,
                                        &src_args,
                                        &self
                                            .unit
                                            .global_scope()
                                            .declared_pointer_types()
                                            .get(
                                                self.template_parameters
                                                    .get(ident.0.as_str())
                                                    .unwrap()
                                                    .typename
                                                    .as_str(),
                                            )
                                            .unwrap(),
                                    );
                                    *e = res;
                                }
                            }
                        } else {
                            debug!("found nested template call to {:?}({:?})", ident, src_args);

                            // The nested template call needs its args substitued for the input
                            // args of the outer template
                            for arg in src_args.iter_mut() {
                                let lambda_expr = match &arg {
                                    Expr::Variable(arg_ident) => {
                                        if let Some(src_arg) = self.subs.get(arg_ident.0.as_str()) {
                                            debug!(
                                                "nested argument function name: {:?}",
                                                arg_ident
                                            );
                                            *arg = (*src_arg).clone();
                                            None
                                        } else if self
                                            .unit
                                            .known_functions()
                                            .contains(arg_ident.0.as_str())
                                        {
                                            debug!("nested raw function name: {:?}", arg_ident);
                                            ident.0 = arg_ident.0.clone();
                                            None
                                        } else {
                                            Some(arg)
                                        }
                                    }
                                    _ => Some(arg),
                                };

                                if let Some(lambda) = lambda_expr {
                                    debug!("nested lambda expression: {:?}", lambda);
                                    // TODO: Is there really nothing to do in this case because of
                                    // the other case being handled before?
                                }
                            }

                            self.instantiator.visit_fun_call(fun, src_args, self.unit);
                        }
                    }

                    return Visit::Parent;
                }

                Visit::Children
            }
        }

        // Perform substitutions
        let mut subs = HashMap::new();
        let mut template_parameters = HashMap::new();

        // Add substitutions for the template parameters
        for (id, (param, value)) in self
            .parameters
            .iter()
            .zip(scope.template_parameters().iter())
            .enumerate()
        {
            if let Some(ps) = &param.symbol {
                subs.insert(ps.as_str(), value);
                template_parameters.insert(ps.as_str(), &self.parameters[id]);
            }
        }

        ast.visit(&mut V {
            subs,
            template_parameters,
            instantiator,
            unit,
        });

        // Change the name
        ast.prototype.name.0 = scope.name().to_owned();

        // Add the captured parameters to the signature
        for ep in scope.captured_parameters() {
            let p = instantiator.get_symbol(ep).unwrap();

            ast.prototype
                .parameters
                .push(FunctionParameterDeclaration::Named(
                    None,
                    FunctionParameterDeclarator {
                        ty: p.decl_type.clone(),
                        ident: ArrayedIdentifier {
                            ident: p.gen_id.clone(),
                            array_spec: p.array.clone(),
                        },
                    },
                ));
        }

        ast.into_node()
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
    pub fn extract_template_parameters(&self, args: &mut Vec<Expr>) -> Result<Vec<Expr>> {
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

        // Return template args with their values
        Ok(res)
    }
}

/// Result of parsing a function definition
pub enum TryTemplate {
    /// GLSLT template function
    Template(TemplateDefinition),
    /// GLSL function
    Function(Node<FunctionDefinition>),
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
    def: Node<FunctionDefinition>,
    declared_pointer_types: &HashMap<String, FunctionPrototype>,
) -> Result<TryTemplate> {
    let mut parameters = Vec::new();
    let mut non_template_parameters = Vec::new();
    let span_id = def.span_id;
    let mut def = def.into_inner();

    for (arg_id, parameter) in def
        .prototype
        .parameters
        .drain(0..def.prototype.parameters.len())
        .enumerate()
    {
        let (n, t) = match &parameter {
            FunctionParameterDeclaration::Named(_, d) => (Some(d.ident.ident.0.clone()), &d.ty),
            FunctionParameterDeclaration::Unnamed(_, t) => (None, t),
        };

        if let TypeSpecifierNonArray::TypeName(tn) = &t.ty {
            if declared_pointer_types.contains_key(&tn.0) {
                if t.array_specifier.is_some() {
                    return Err(Error::ArrayedTemplateParameter {
                        name: def.prototype.name.to_string(),
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

    if parameters.is_empty() {
        Ok(TryTemplate::Function(Node::new(def, span_id)))
    } else {
        Ok(TryTemplate::Template(TemplateDefinition {
            ast: def,
            parameters,
            span_id,
        }))
    }
}
