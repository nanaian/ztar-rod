use std::fmt::Write;
use std::collections::{VecDeque, HashMap};
use std::cell::RefCell;
use failure_derive::*;
use crate::rom::{Rom, Map};

pub mod datatype;
pub mod bc;
mod globals;
pub mod parse;

use datatype::*;
use parse::{ast::*, Unparse};

pub fn decompile_map(map: Map, _rom: &mut Rom) -> Result<String, Error> {
    let mut scope        = Scope::new();
    let mut declarations = Vec::new();

    // Bring global methods into scope
    for (ptr, name, ty) in &*globals::METHODS {
        scope.insert_ptr(*ptr, name.to_string(), ty.clone());
    }

    {
        let (loc, bc) = map.main_fun;

        // Main function takes no arguments
        scope.insert_ptr(loc.into(), "main".to_string(), DataType::Fun(vec![]));

        // Decompile the bytecode
        let mut decl = Declaration::Fun {
            name:      IdentifierOrPointer::Pointer(loc.into()),
            arguments: Vec::new(),
            block:     bc.decompile(&mut scope)?,
        };

        for mut block in decl.inner_blocks_mut() {
            // TODO: decompile pointers within, followed by a type inference pass

            fix_call_arg_capture(&mut block, &scope)?;
            infer_datatypes(&mut block, &mut scope)?;
        }

        // TODO: replace decl.arguments with the types that were inferred

        declarations.push(decl);
    }

    // Unparse everything
    let mut out = String::new();

    for declaration in declarations.into_iter() {
        writeln!(out, "{}", declaration.unparse(&scope)).unwrap();
    }

    Ok(out)
}

/// Paper Mario function calls capture their environment -- that is, they take
/// every single FunWord/FunFlag as an argument by default. This fixes method
/// calls to do just that depending on the function signature defined in the
/// given Scope. This fn expects that all function signatures are correctly
/// defined in-scope.
///
/// For example, entry_walk takes a single argument, so the following:
///
///     callback = myscript
///     entry_walk()
///
/// Would be transformed into:
///
///     callback = myscript
///     entry_walk(callback)
///
/// Note that this transformation should only be applied to decompiled ASTs, not
/// those the user gives us; this should be a missing-method-arg error.
fn fix_call_arg_capture(block: &mut Vec<Statement>, scope: &Scope) -> Result<(), Error> {
    for stmt in block.iter_mut() {
        if let Statement::MethodCall { method, arguments, .. } = stmt {
            // Only functions capture - asm methods take args normally.
            if let Some((_, DataType::Fun(argument_types))) = method.lookup(scope) {
                assert_eq!(arguments.len(), 0);

                for (n, _) in argument_types.iter().enumerate() {
                    // TODO: see if FunFlags should be captured if the arg type
                    //       is DataType::Bool

                    let name = format!("{}_{:X}", globals::FUNWORD_STR, n);

                    arguments.push(RefCell::new(Expression::Identifier(Identifier(name))));
                }
            }
        }

        // Fix inner blocks, too.
        for mut inner_block in stmt.inner_blocks_mut() {
            fix_call_arg_capture(&mut inner_block, &scope)?;
        }
    }

    Ok(())
}

/// Performs a single type inference pass. Replaces 'any' declarations and their
/// respective scope mappings if their types can be inferred.
fn infer_datatypes(block: &mut Vec<Statement>, mut scope: &mut Scope) -> Result<(), Error> {
    let mut made_inferences = true;

    // This works like a bubble sort -- keep inferring types until we can't.
    while made_inferences {
        made_inferences = false;

        // We only insert inferred types into scope after the interator
        // finishes, because we perform lookups in there and the borrow checker
        // would scream at us for mutating it while we had an immutable ref.
        let mut inferred: Vec<(String, DataType)> = Vec::new();

        // We iterate in reverse so we can figure out the types before we see their
        // declaration statement (once we do see it, we update its type).
        for stmt in block.iter_mut().rev() {
            match stmt {
                // Update var declarations with inferred types.
                Statement::VarDeclare { datatype, identifier: Identifier(name), expression } => {
                    match scope.lookup_name_depth(&name, 0) {
                        Some(inferred_datatype) => match datatype.replace(DataType::Any) {
                            // User has left it up to the compiler to infer the
                            // type, so lets do that.
                            DataType::Any => {
                                datatype.replace(inferred_datatype.clone());

                                if let DataType::Bool = inferred_datatype {
                                    // Update int literal to a bool literal.
                                    if let Some(expression) = expression {
                                        if let Expression::LiteralInt(v) = expression.clone().into_inner() {
                                            expression.replace(Expression::LiteralBool(v == 1));
                                        }
                                    }
                                }
                            },

                            // User declared the type but we inferred its use
                            // as some other type. Error.
                            datatype => return Err(Error::VarDeclareTypeMismatch {
                                identifier:        name.clone(),
                                declared_datatype: datatype,
                                inferred_datatype: inferred_datatype.clone(),
                            }),
                        },

                        // The variable is declared here but isn't in the current
                        // scope, so add it to the scope after this pass.
                        None => inferred.push((name.clone(), match expression {
                            Some(expression) => expression.borrow().infer_datatype(&scope),
                            None             => DataType::Any,
                        })),
                    }
                },

                // Infer left-hand-type by the right-hand-type of var assignments.
                Statement::VarAssign { identifier: Identifier(name), expression } => {
                    match scope.lookup_name(name) {
                        // We only need to infer Any (i.e. unknown) types.
                        Some(DataType::Any)
                            => inferred.push((name.clone(), expression.borrow().infer_datatype(scope))),

                        // Update int literal to bool literal.
                        Some(DataType::Bool) => {
                            if let Expression::LiteralInt(v) = expression.clone().into_inner() {
                                expression.replace(Expression::LiteralBool(v == 1));
                            }
                        },

                        _ => (),
                    }
                },

                // Infer types of method call arguments.
                Statement::MethodCall { method, arguments, .. } => match method.lookup(scope) {
                    Some((_, &DataType::Asm(ref arg_types))) |
                    Some((_, &DataType::Fun(ref arg_types))) => {
                        for (ty, arg) in arg_types.iter().zip(arguments.iter()) {
                            match arg.clone().into_inner() {
                                // Only identifiers influence type inference.
                                Expression::Identifier(Identifier(name)) => {
                                    // We only need to infer Any (i.e. unknown) types.
                                    if let Some(DataType::Any) = scope.lookup_name(&name) {
                                        // Define the inferred type!
                                        inferred.push((name.clone(), ty.clone()));
                                    }
                                },

                                // Update int literal to bool literal.
                                Expression::LiteralInt(v) => {
                                    if let DataType::Bool = ty {
                                        arg.replace(Expression::LiteralBool(v == 1));
                                    }
                                },

                                _ => (),
                            }
                        }
                    },

                    _ => (),
                },

                _ => (),
            }

            for mut inner_block in stmt.inner_blocks_mut() {
                infer_datatypes(&mut inner_block, &mut scope)?;
            }
        }

        // Define the inferred types in-scope.
        for (name, datatype) in inferred.into_iter() {
            if let DataType::Any = datatype {
                // ...why is this even here?
                break
            }

            match scope.insert_name(name, datatype) {
                Some(DataType::Any) => (),
                Some(_) => panic!("type inferred but var has a known type already"),
                None => (),
            }

            made_inferences = true
        }
    }

    Ok(())
}

#[derive(Debug, Fail)]
pub enum Error {
    #[fail(display = "failed to decompile bytecode: {}", _0)]
    BytecodeDecompile(#[fail(cause)] bc::Error),

    #[fail(display = "variable '{}' declared as {} but is used as {}",
        identifier, declared_datatype, inferred_datatype)]
    VarDeclareTypeMismatch {
        identifier:    String,
        declared_datatype: DataType,
        inferred_datatype: DataType,
    },
}

impl From<bc::Error> for Error {
    fn from(error: bc::Error) -> Error {
        Error::BytecodeDecompile(error)
    }
}

/// A priority-queue mapping of (u32 -> String -> DataType); i.e. Scope provides
/// lookups of pointer-to-name and name-to-datatype, preferring the current
/// scope (see `push` and `pop`) when performing lookups.
#[derive(Debug)]
pub struct Scope {
    layers: VecDeque<(HashMap<u32, String>, HashMap<String, DataType>)>,
}

impl Scope {
    /// Creates a new Scope.
    pub fn new() -> Scope {
        let mut scope = Scope { layers: VecDeque::new() };
        scope.push();
        scope
    }

    /// Adds a new mapping on-top of the current scope. Values inserted
    /// will 'shadow' (soft-overwrite) values below with the same key until
    /// this scope is popped.
    pub fn push(&mut self) {
        self.layers.push_front((HashMap::new(), HashMap::new()));
    }

    /// Removes the current scope mapping and returns it, if any.
    pub fn pop(&mut self) -> Option<(HashMap<u32, String>, HashMap<String, DataType>)> {
        self.layers.pop_front()
    }

    /// Inserts a (u32 -> String -> DataType) mapping. If the current scope
    /// already has either key, they are updated and their previous datatype
    /// is returned.
    pub fn insert_ptr(&mut self, ptr: u32, name: String, datatype: DataType) -> Option<DataType> {
        self.layers[0].0.insert(ptr, name.clone());
        self.layers[0].1.insert(name, datatype)
    }

    /// Inserts a (String -> DataType) mapping. If the current scope already has
    /// this name mapped, it is updated and its previous datatype returned.
    pub fn insert_name(&mut self, name: String, datatype: DataType) -> Option<DataType> {
        self.layers[0].1.insert(name, datatype)
    }

    /// Looks-up the name associated with a given pointer.
    pub fn lookup_ptr(&self, ptr: u32) -> Option<&str> {
        for layer in self.layers.iter() {
            if let Some(name) = layer.0.get(&ptr) {
                return Some(name);
            }
        }

        None
    }

    /// Looks-up the datatype associated with a given name.
    pub fn lookup_name(&self, name: &str) -> Option<&DataType> {
        for layer in self.layers.iter() {
            if let Some(datatype) = layer.1.get(name) {
                return Some(datatype);
            }
        }

        None
    }

    /// Looks-up the datatype associated with a given name to a given depth.
    /// For example, providing a max depth of 0 would only search the current
    /// scope's mapping.
    pub fn lookup_name_depth(&self, name: &str, max_depth: usize) -> Option<&DataType> {
        for layer in self.layers.iter().take(max_depth + 1) {
            if let Some(datatype) = layer.1.get(name) {
                return Some(datatype);
            }
        }

        None
    }
}
