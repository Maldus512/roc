// This program was written by Jelle Teeuwissen within a final
// thesis project of the Computing Science master program at Utrecht
// University under supervision of Wouter Swierstra (w.s.swierstra@uu.nl).

// Implementation based of Drop Specialization from Perceus: Garbage Free Reference Counting with Reuse
// https://www.microsoft.com/en-us/research/uploads/prod/2021/06/perceus-pldi21.pdf

#![allow(clippy::too_many_arguments)]

use std::cmp::{self, Ord};
use std::iter::Iterator;

use bumpalo::collections::vec::Vec;
use bumpalo::collections::CollectIn;

use roc_module::low_level::LowLevel;
use roc_module::symbol::{IdentIds, ModuleId, Symbol};
use roc_target::TargetInfo;

use crate::ir::{
    BranchInfo, Call, CallType, Expr, JoinPointId, Literal, ModifyRc, Proc, ProcLayout, Stmt,
    UpdateModeId,
};
use crate::layout::{
    Builtin, InLayout, Layout, LayoutInterner, LayoutRepr, STLayoutInterner, UnionLayout,
};

use bumpalo::Bump;

use roc_collections::{MutMap, MutSet};

/**
Try to find increments of symbols followed by decrements of the symbol they were indexed out of (their parent).
Then inline the decrement operation of the parent and removing matching pairs of increments and decrements.
*/
pub fn specialize_drops<'a, 'i>(
    arena: &'a Bump,
    layout_interner: &'i mut STLayoutInterner<'a>,
    home: ModuleId,
    ident_ids: &'i mut IdentIds,
    target_info: TargetInfo,
    procs: &mut MutMap<(Symbol, ProcLayout<'a>), Proc<'a>>,
) {
    for ((_symbol, proc_layout), proc) in procs.iter_mut() {
        let mut environment =
            DropSpecializationEnvironment::new(arena, home, proc_layout.result, target_info);
        specialize_drops_proc(arena, layout_interner, ident_ids, &mut environment, proc);
    }
}

fn specialize_drops_proc<'a, 'i>(
    arena: &'a Bump,
    layout_interner: &'i mut STLayoutInterner<'a>,
    ident_ids: &'i mut IdentIds,
    environment: &mut DropSpecializationEnvironment<'a>,
    proc: &mut Proc<'a>,
) {
    for (layout, symbol) in proc.args.iter().copied() {
        environment.add_symbol_layout(symbol, layout);
    }

    let new_body =
        specialize_drops_stmt(arena, layout_interner, ident_ids, environment, &proc.body);

    proc.body = new_body.clone();
}

fn specialize_drops_stmt<'a, 'i>(
    arena: &'a Bump,
    layout_interner: &'i mut STLayoutInterner<'a>,
    ident_ids: &'i mut IdentIds,
    environment: &mut DropSpecializationEnvironment<'a>,
    stmt: &Stmt<'a>,
) -> &'a Stmt<'a> {
    match stmt {
        Stmt::Let(binding, expr, layout, continuation) => {
            environment.add_symbol_layout(*binding, *layout);

            macro_rules! alloc_let_with_continuation {
                ($environment:expr) => {{
                    let new_continuation = specialize_drops_stmt(
                        arena,
                        layout_interner,
                        ident_ids,
                        $environment,
                        continuation,
                    );
                    arena.alloc(Stmt::Let(*binding, expr.clone(), *layout, new_continuation))
                }};
            }

            match expr {
                Expr::Call(Call {
                    call_type,
                    arguments,
                }) => {
                    match call_type.clone().replace_lowlevel_wrapper() {
                        CallType::LowLevel {
                            op: LowLevel::ListGetUnsafe,
                            ..
                        } => {
                            let [structure, index] = match arguments {
                                [structure, index] => [structure, index],
                                _ => unreachable!("List get should have two arguments"),
                            };

                            environment.add_list_child(*structure, *binding, index);

                            alloc_let_with_continuation!(environment)
                        }
                        _ => {
                            // TODO perhaps allow for some e.g. lowlevel functions to be called if they cannot modify the RC of the symbol.

                            // Calls can modify the RC of the symbol.
                            // If we move a increment of children after the function,
                            // the function might deallocate the child before we can use it after the function.
                            // If we move the decrement of the parent to before the function,
                            // the parent might be deallocated before the function can use it.
                            // Thus forget everything about any increments.

                            let mut new_environment = environment.clone_without_incremented();

                            alloc_let_with_continuation!(&mut new_environment)
                        }
                    }
                }
                Expr::Struct(_) => {
                    let mut new_environment = environment.clone_without_incremented();

                    alloc_let_with_continuation!(&mut new_environment)
                }
                Expr::Tag { tag_id, .. } => {
                    let mut new_environment = environment.clone_without_incremented();

                    new_environment.symbol_tag.insert(*binding, *tag_id);

                    alloc_let_with_continuation!(&mut new_environment)
                }
                Expr::StructAtIndex {
                    index, structure, ..
                } => {
                    environment.add_struct_child(*structure, *binding, *index);
                    // alloc_let_with_continuation!(environment)

                    // TODO do we need to remove the indexed value to prevent it from being dropped sooner?
                    // It will only be dropped sooner if the reference count is 1. Which can only happen if there is no increment before.
                    // So we should be fine.
                    alloc_let_with_continuation!(environment)
                }
                Expr::UnionAtIndex {
                    structure,
                    tag_id,
                    union_layout: _,
                    index,
                } => {
                    // TODO perhaps we need the union_layout later as well? if so, create a new function/map to store it.
                    environment.add_union_child(*structure, *binding, *tag_id, *index);
                    // Generated code might know the tag of the union without switching on it.
                    // So if we unionAtIndex, we must know the tag and we can use it to specialize the drop.
                    environment.symbol_tag.insert(*structure, *tag_id);
                    alloc_let_with_continuation!(environment)
                }
                Expr::ExprUnbox { symbol } => {
                    environment.add_box_child(*symbol, *binding);
                    alloc_let_with_continuation!(environment)
                }

                Expr::Reuse { .. } => {
                    alloc_let_with_continuation!(environment)
                }
                Expr::Reset { .. } => {
                    // TODO allow to inline this to replace it with resetref
                    alloc_let_with_continuation!(environment)
                }
                Expr::ResetRef { .. } => {
                    alloc_let_with_continuation!(environment)
                }
                Expr::Literal(literal) => {
                    // literal ints are used to store the the index for lists.
                    // Add it to the env so when we use it to index a list, we can use the index to specialize the drop.
                    if let Literal::Int(i) = literal {
                        environment
                            .symbol_index
                            .insert(*binding, i128::from_ne_bytes(*i) as u64);
                    }
                    alloc_let_with_continuation!(environment)
                }

                Expr::RuntimeErrorFunction(_)
                | Expr::ExprBox { .. }
                | Expr::NullPointer
                | Expr::GetTagId { .. }
                | Expr::EmptyArray
                | Expr::Array { .. } => {
                    // Does nothing relevant to drop specialization. So we can just continue.
                    alloc_let_with_continuation!(environment)
                }
            }
        }
        Stmt::Switch {
            cond_symbol,
            cond_layout,
            branches,
            default_branch,
            ret_layout,
        } => {
            macro_rules! insert_branch_info {
                ($branch_env:expr,$info:expr ) => {
                    match $info {
                        BranchInfo::Constructor {
                            scrutinee: symbol,
                            tag_id: tag,
                            ..
                        } => {
                            $branch_env.symbol_tag.insert(*symbol, *tag);
                        }
                        BranchInfo::List {
                            scrutinee: symbol,
                            len,
                        } => {
                            $branch_env.list_length.insert(*symbol, *len);
                        }
                        _ => (),
                    }
                };
            }

            let new_branches = branches
                .iter()
                .map(|(label, info, branch)| {
                    let mut branch_env = environment.clone_without_incremented();

                    insert_branch_info!(branch_env, info);

                    let new_branch = specialize_drops_stmt(
                        arena,
                        layout_interner,
                        ident_ids,
                        &mut branch_env,
                        branch,
                    );

                    (*label, info.clone(), new_branch.clone())
                })
                .collect_in::<Vec<_>>(arena)
                .into_bump_slice();

            let new_default_branch = {
                let (info, branch) = default_branch;

                let mut branch_env = environment.clone_without_incremented();

                insert_branch_info!(branch_env, info);

                let new_branch = specialize_drops_stmt(
                    arena,
                    layout_interner,
                    ident_ids,
                    &mut branch_env,
                    branch,
                );

                (info.clone(), new_branch)
            };

            arena.alloc(Stmt::Switch {
                cond_symbol: *cond_symbol,
                cond_layout: *cond_layout,
                branches: new_branches,
                default_branch: new_default_branch,
                ret_layout: *ret_layout,
            })
        }
        Stmt::Ret(symbol) => arena.alloc(Stmt::Ret(*symbol)),
        Stmt::Refcounting(rc, continuation) => match rc {
            ModifyRc::Inc(symbol, count) => {
                let any = environment.any_incremented(symbol);

                // Add a symbol for every increment performed.
                environment.add_incremented(*symbol, *count);

                let new_continuation = specialize_drops_stmt(
                    arena,
                    layout_interner,
                    ident_ids,
                    environment,
                    continuation,
                );

                if any {
                    // There were increments before this one, best to let the first one do the increments.
                    // Or there are no increments left, so we can just continue.
                    new_continuation
                } else {
                    match environment.get_incremented(symbol) {
                        // This is the first increment, but all increments are consumed. So don't insert any.
                        0 => new_continuation,
                        // We still need to do some increments.
                        new_count => arena.alloc(Stmt::Refcounting(
                            ModifyRc::Inc(*symbol, new_count),
                            new_continuation,
                        )),
                    }
                }
            }
            ModifyRc::Dec(symbol) => {
                // We first check if there are any outstanding increments we can cross of with this decrement.
                // Then we check the continuation, since it might have a decrement of a symbol that's a child of this one.
                // Afterwards we perform drop specialization.
                // In the following example, we don't want to inline `dec b`, we want to remove the `inc a` and `dec a` instead.
                // let a = index b
                // inc a
                // dec a
                // dec b

                if environment.pop_incremented(symbol) {
                    // This decremented symbol was incremented before, so we can remove it.
                    specialize_drops_stmt(
                        arena,
                        layout_interner,
                        ident_ids,
                        environment,
                        continuation,
                    )
                } else {
                    // Collect all children that were incremented and make sure that one increment remains in the environment afterwards.
                    // To prevent
                    // let a = index b; inc a; dec b; ...; dec a
                    // from being translated to
                    // let a = index b; dec b
                    // As a might get dropped as a result of the decrement of b.
                    let mut incremented_children = environment
                        .get_children(symbol)
                        .iter()
                        .copied()
                        .filter_map(|child| environment.pop_incremented(&child).then_some(child))
                        .collect::<MutSet<_>>();

                    // This decremented symbol was not incremented before, perhaps the children were.
                    let in_layout = environment.get_symbol_layout(symbol);
                    let runtime_layout = layout_interner.runtime_representation(*in_layout);

                    let new_dec = match runtime_layout.repr {
                        // Layout has children, try to inline them.
                        LayoutRepr::Struct { field_layouts, .. } => specialize_struct(
                            arena,
                            layout_interner,
                            ident_ids,
                            environment,
                            symbol,
                            field_layouts,
                            &mut incremented_children,
                            continuation,
                        ),
                        LayoutRepr::Union(union_layout) => specialize_union(
                            arena,
                            layout_interner,
                            ident_ids,
                            environment,
                            symbol,
                            union_layout,
                            &mut incremented_children,
                            continuation,
                        ),
                        LayoutRepr::Boxed(_layout) => specialize_boxed(
                            arena,
                            layout_interner,
                            ident_ids,
                            environment,
                            &mut incremented_children,
                            symbol,
                            continuation,
                        ),
                        LayoutRepr::Builtin(Builtin::List(layout)) => specialize_list(
                            arena,
                            layout_interner,
                            ident_ids,
                            environment,
                            &mut incremented_children,
                            symbol,
                            layout,
                            continuation,
                        ),
                        // TODO: lambda sets should not be reachable, yet they are.
                        _ => {
                            let new_continuation = specialize_drops_stmt(
                                arena,
                                layout_interner,
                                ident_ids,
                                environment,
                                continuation,
                            );

                            // No children, keep decrementing the symbol.
                            arena.alloc(Stmt::Refcounting(ModifyRc::Dec(*symbol), new_continuation))
                        }
                    };

                    // Add back the increments for the children to the environment.
                    for child_symbol in incremented_children.iter() {
                        environment.add_incremented(*child_symbol, 1)
                    }

                    new_dec
                }
            }
            ModifyRc::DecRef(_) => {
                // Inlining has no point, since it doesn't decrement it's children
                arena.alloc(Stmt::Refcounting(
                    *rc,
                    specialize_drops_stmt(
                        arena,
                        layout_interner,
                        ident_ids,
                        environment,
                        continuation,
                    ),
                ))
            }
        },
        Stmt::Expect {
            condition,
            region,
            lookups,
            variables,
            remainder,
        } => arena.alloc(Stmt::Expect {
            condition: *condition,
            region: *region,
            lookups,
            variables,
            remainder: specialize_drops_stmt(
                arena,
                layout_interner,
                ident_ids,
                environment,
                remainder,
            ),
        }),
        Stmt::ExpectFx {
            condition,
            region,
            lookups,
            variables,
            remainder,
        } => arena.alloc(Stmt::ExpectFx {
            condition: *condition,
            region: *region,
            lookups,
            variables,
            remainder: specialize_drops_stmt(
                arena,
                layout_interner,
                ident_ids,
                environment,
                remainder,
            ),
        }),
        Stmt::Dbg {
            symbol,
            variable,
            remainder,
        } => arena.alloc(Stmt::Dbg {
            symbol: *symbol,
            variable: *variable,
            remainder: specialize_drops_stmt(
                arena,
                layout_interner,
                ident_ids,
                environment,
                remainder,
            ),
        }),
        Stmt::Join {
            id,
            parameters,
            body,
            remainder,
        } => {
            let mut new_environment = environment.clone_without_incremented();

            for param in parameters.iter() {
                new_environment.add_symbol_layout(param.symbol, param.layout);
            }

            let new_body = specialize_drops_stmt(
                arena,
                layout_interner,
                ident_ids,
                &mut new_environment,
                body,
            );

            arena.alloc(Stmt::Join {
                id: *id,
                parameters,
                body: new_body,
                remainder: specialize_drops_stmt(
                    arena,
                    layout_interner,
                    ident_ids,
                    environment,
                    remainder,
                ),
            })
        }
        Stmt::Jump(joinpoint_id, arguments) => arena.alloc(Stmt::Jump(*joinpoint_id, arguments)),
        Stmt::Crash(symbol, crash_tag) => arena.alloc(Stmt::Crash(*symbol, *crash_tag)),
    }
}

fn specialize_struct<'a, 'i>(
    arena: &'a Bump,
    layout_interner: &'i mut STLayoutInterner<'a>,
    ident_ids: &'i mut IdentIds,
    environment: &mut DropSpecializationEnvironment<'a>,
    symbol: &Symbol,
    struct_layout: &'a [InLayout],
    incremented_children: &mut MutSet<Child>,
    continuation: &'a Stmt<'a>,
) -> &'a Stmt<'a> {
    match environment.struct_children.get(symbol) {
        // TODO all these children might be non reference counting, inlining the dec without any benefit.
        // Perhaps only insert children that are reference counted.
        Some(children) => {
            // TODO perhaps this allocation can be avoided.
            let children_clone = children.clone();

            // Map tracking which index of the struct is contained in which symbol.
            // And whether the child no longer has to be decremented.
            let mut index_symbols = MutMap::default();

            for (index, _layout) in struct_layout.iter().enumerate() {
                for (child, _i) in children_clone.iter().filter(|(_, i)| *i == index as u64) {
                    let removed = incremented_children.remove(child);
                    index_symbols.insert(index, (*child, removed));

                    if removed {
                        break;
                    }
                }
            }

            let mut new_continuation =
                specialize_drops_stmt(arena, layout_interner, ident_ids, environment, continuation);

            // Make sure every field is decremented.
            // Reversed to ensure that the generated code decrements the fields in the correct order.
            for (i, field_layout) in struct_layout.iter().enumerate().rev() {
                // Only insert decrements for fields that are/contain refcounted values.
                if layout_interner.contains_refcounted(*field_layout) {
                    new_continuation = match index_symbols.get(&i) {
                        // This value has been indexed before, use that symbol.
                        Some((s, popped)) => {
                            if *popped {
                                // This symbol was popped, so we can skip the decrement.
                                new_continuation
                            } else {
                                // This symbol was indexed but not decremented, so we will decrement it.
                                arena.alloc(Stmt::Refcounting(ModifyRc::Dec(*s), new_continuation))
                            }
                        }

                        // This value has not been index before, create a new symbol.
                        None => {
                            let field_symbol =
                                environment.create_symbol(ident_ids, &format!("field_val_{}", i));

                            let field_val_expr = Expr::StructAtIndex {
                                index: i as u64,
                                field_layouts: struct_layout,
                                structure: *symbol,
                            };

                            arena.alloc(Stmt::Let(
                                field_symbol,
                                field_val_expr,
                                layout_interner.chase_recursive_in(*field_layout),
                                arena.alloc(Stmt::Refcounting(
                                    ModifyRc::Dec(field_symbol),
                                    new_continuation,
                                )),
                            ))
                        }
                    };
                }
            }

            new_continuation
        }
        None => {
            // No known children, keep decrementing the symbol.
            let new_continuation =
                specialize_drops_stmt(arena, layout_interner, ident_ids, environment, continuation);

            arena.alloc(Stmt::Refcounting(ModifyRc::Dec(*symbol), new_continuation))
        }
    }
}

fn specialize_union<'a, 'i>(
    arena: &'a Bump,
    layout_interner: &'i mut STLayoutInterner<'a>,
    ident_ids: &'i mut IdentIds,
    environment: &mut DropSpecializationEnvironment<'a>,
    symbol: &Symbol,
    union_layout: UnionLayout<'a>,
    incremented_children: &mut MutSet<Child>,
    continuation: &'a Stmt<'a>,
) -> &'a Stmt<'a> {
    let current_tag = environment.symbol_tag.get(symbol).copied();

    macro_rules! keep_original_decrement {
        () => {{
            let new_continuation =
                specialize_drops_stmt(arena, layout_interner, ident_ids, environment, continuation);
            arena.alloc(Stmt::Refcounting(ModifyRc::Dec(*symbol), new_continuation))
        }};
    }

    match get_union_tag_layout(union_layout, current_tag) {
        // No known tag, decrement the symbol as usual.
        UnionFieldLayouts::Unknown => {
            keep_original_decrement!()
        }

        // The union is null, so we can skip the decrement.
        UnionFieldLayouts::Null => {
            specialize_drops_stmt(arena, layout_interner, ident_ids, environment, continuation)
        }

        // We know the tag, we can specialize the decrement for the tag.
        UnionFieldLayouts::Found { field_layouts, tag } => {
            match environment.union_children.get(symbol) {
                None => keep_original_decrement!(),
                Some(children) => {
                    // TODO perhaps this allocation can be avoided.
                    let children_clone = children.clone();

                    // Map tracking which index of the struct is contained in which symbol.
                    // And whether the child no longer has to be decremented.
                    let mut index_symbols = MutMap::default();

                    for (index, _layout) in field_layouts.iter().enumerate() {
                        for (child, t, _i) in children_clone
                            .iter()
                            .filter(|(_child, _t, i)| *i == index as u64)
                        {
                            debug_assert_eq!(tag, *t);

                            let removed = incremented_children.remove(child);
                            index_symbols.insert(index, (*child, removed));

                            if removed {
                                break;
                            }
                        }
                    }

                    let new_continuation = specialize_drops_stmt(
                        arena,
                        layout_interner,
                        ident_ids,
                        environment,
                        continuation,
                    );

                    type RCFun<'a> =
                        Option<fn(arena: &'a Bump, Symbol, &'a Stmt<'a>) -> &'a Stmt<'a>>;
                    let refcount_fields = |layout_interner: &mut STLayoutInterner<'a>,
                                           ident_ids: &mut IdentIds,
                                           rc_popped: RCFun<'a>,
                                           rc_unpopped: RCFun<'a>,
                                           continuation: &'a Stmt<'a>|
                     -> &'a Stmt<'a> {
                        let mut new_continuation = continuation;

                        // Reversed to ensure that the generated code decrements the fields in the correct order.
                        for (i, field_layout) in field_layouts.iter().enumerate().rev() {
                            // Only insert decrements for fields that are/contain refcounted values.
                            if layout_interner.contains_refcounted(*field_layout) {
                                new_continuation = match index_symbols.get(&i) {
                                    // This value has been indexed before, use that symbol.
                                    Some((s, popped)) => {
                                        if *popped {
                                            // This symbol was popped, so we can skip the decrement.
                                            match rc_popped {
                                                Some(rc) => rc(arena, *s, new_continuation),
                                                None => new_continuation,
                                            }
                                        } else {
                                            // This symbol was indexed but not decremented, so we will decrement it.
                                            match rc_unpopped {
                                                Some(rc) => rc(arena, *s, new_continuation),
                                                None => new_continuation,
                                            }
                                        }
                                    }

                                    // This value has not been index before, create a new symbol.
                                    None => match rc_unpopped {
                                        Some(rc) => {
                                            let field_symbol = environment.create_symbol(
                                                ident_ids,
                                                &format!("field_val_{}", i),
                                            );

                                            let field_val_expr = Expr::UnionAtIndex {
                                                structure: *symbol,
                                                tag_id: tag,
                                                union_layout,
                                                index: i as u64,
                                            };

                                            arena.alloc(Stmt::Let(
                                                field_symbol,
                                                field_val_expr,
                                                layout_interner.chase_recursive_in(*field_layout),
                                                rc(arena, field_symbol, new_continuation),
                                            ))
                                        }
                                        None => new_continuation,
                                    },
                                };
                            }
                        }

                        new_continuation
                    };

                    match union_layout {
                        UnionLayout::NonRecursive(_) => refcount_fields(
                            layout_interner,
                            ident_ids,
                            // Do nothing for the children that were incremented before, as the decrement will cancel out.
                            None,
                            // Decrement the children that were not incremented before. And thus don't cancel out.
                            Some(|arena, symbol, continuation| {
                                arena.alloc(Stmt::Refcounting(ModifyRc::Dec(symbol), continuation))
                            }),
                            new_continuation,
                        ),
                        UnionLayout::Recursive(_)
                        | UnionLayout::NonNullableUnwrapped(_)
                        | UnionLayout::NullableWrapped { .. }
                        | UnionLayout::NullableUnwrapped { .. } => {
                            branch_uniqueness(
                                arena,
                                ident_ids,
                                layout_interner,
                                environment,
                                *symbol,
                                // If the symbol is unique:
                                // - drop the children that were not incremented before
                                // - don't do anything for the children that were incremented before
                                // - free the parent
                                |layout_interner, ident_ids, continuation| {
                                    refcount_fields(
                                        layout_interner,
                                        ident_ids,
                                        // Do nothing for the children that were incremented before, as the decrement will cancel out.
                                        None,
                                        // Decrement the children that were not incremented before. And thus don't cancel out.
                                        Some(|arena, symbol, continuation| {
                                            arena.alloc(Stmt::Refcounting(
                                                ModifyRc::Dec(symbol),
                                                continuation,
                                            ))
                                        }),
                                        arena.alloc(Stmt::Refcounting(
                                            // TODO this could be replaced by a free if ever added to the IR.
                                            ModifyRc::DecRef(*symbol),
                                            continuation,
                                        )),
                                    )
                                },
                                // If the symbol is not unique:
                                // - increment the children that were incremented before
                                // - don't do anything for the children that were not incremented before
                                // - decref the parent
                                |layout_interner, ident_ids, continuation| {
                                    refcount_fields(
                                        layout_interner,
                                        ident_ids,
                                        Some(|arena, symbol, continuation| {
                                            arena.alloc(Stmt::Refcounting(
                                                ModifyRc::Inc(symbol, 1),
                                                continuation,
                                            ))
                                        }),
                                        None,
                                        arena.alloc(Stmt::Refcounting(
                                            ModifyRc::DecRef(*symbol),
                                            continuation,
                                        )),
                                    )
                                },
                                new_continuation,
                            )
                        }
                    }
                }
            }
        }
    }
}

fn specialize_boxed<'a, 'i>(
    arena: &'a Bump,
    layout_interner: &'i mut STLayoutInterner<'a>,
    ident_ids: &'i mut IdentIds,
    environment: &mut DropSpecializationEnvironment<'a>,
    incremented_children: &mut MutSet<Child>,
    symbol: &Symbol,
    continuation: &'a Stmt<'a>,
) -> &'a mut Stmt<'a> {
    let removed = match incremented_children.iter().next() {
        Some(s) => incremented_children.remove(&s.clone()),
        None => false,
    };

    let new_continuation =
        specialize_drops_stmt(arena, layout_interner, ident_ids, environment, continuation);

    if removed {
        // No need to decrement the containing value since we already decremented the child.
        arena.alloc(Stmt::Refcounting(
            ModifyRc::DecRef(*symbol),
            new_continuation,
        ))
    } else {
        // No known children, keep decrementing the symbol.
        arena.alloc(Stmt::Refcounting(ModifyRc::Dec(*symbol), new_continuation))
    }
}

fn specialize_list<'a, 'i>(
    arena: &'a Bump,
    layout_interner: &'i mut STLayoutInterner<'a>,
    ident_ids: &'i mut IdentIds,
    environment: &mut DropSpecializationEnvironment<'a>,
    incremented_children: &mut MutSet<Child>,
    symbol: &Symbol,
    item_layout: InLayout,
    continuation: &'a Stmt<'a>,
) -> &'a Stmt<'a> {
    let current_length = environment.list_length.get(symbol).copied();

    macro_rules! keep_original_decrement {
        () => {{
            let new_continuation =
                specialize_drops_stmt(arena, layout_interner, ident_ids, environment, continuation);
            arena.alloc(Stmt::Refcounting(ModifyRc::Dec(*symbol), new_continuation))
        }};
    }

    match (
        layout_interner.contains_refcounted(item_layout),
        current_length,
    ) {
        (true, Some(length)) => {
            match environment.list_children.get(symbol) {
                // Only specialize lists if all children are known.
                // Otherwise we might have to insert an unbouned number of decrements.
                Some(children) if children.len() as u64 == length => {
                    // TODO perhaps this allocation can be avoided.
                    let children_clone = children.clone();

                    // Map tracking which index of the struct is contained in which symbol.
                    // And whether the child no longer has to be decremented.
                    let mut index_symbols = MutMap::default();

                    for index in 0..length {
                        for (child, i) in children_clone.iter().filter(|(_child, i)| *i == index) {
                            debug_assert!(length > *i);

                            let removed = incremented_children.remove(child);
                            index_symbols.insert(index, (*child, removed));

                            if removed {
                                break;
                            }
                        }
                    }

                    let new_continuation = specialize_drops_stmt(
                        arena,
                        layout_interner,
                        ident_ids,
                        environment,
                        continuation,
                    );

                    let mut newer_continuation = arena.alloc(Stmt::Refcounting(
                        ModifyRc::DecRef(*symbol),
                        new_continuation,
                    ));

                    // Reversed to ensure that the generated code decrements the items in the correct order.
                    for i in (0..length).rev() {
                        let (s, popped) = index_symbols.get(&i).unwrap();

                        if !*popped {
                            // Decrement the children that were not incremented before. And thus don't cancel out.
                            newer_continuation = arena
                                .alloc(Stmt::Refcounting(ModifyRc::Dec(*s), newer_continuation));
                        }

                        // Do nothing for the children that were incremented before, as the decrement will cancel out.
                    }

                    newer_continuation
                }
                _ => keep_original_decrement!(),
            }
        }
        _ => {
            // List length is unknown or the children are not reference counted, so we can't specialize.
            keep_original_decrement!()
        }
    }
}

/**
Get the field layouts of a union given a tag.
*/
fn get_union_tag_layout(union_layout: UnionLayout<'_>, tag: Option<Tag>) -> UnionFieldLayouts {
    match (union_layout, tag) {
        (UnionLayout::NonRecursive(union_layouts), Some(tag)) => UnionFieldLayouts::Found {
            field_layouts: union_layouts[tag as usize],
            tag,
        },
        (UnionLayout::Recursive(union_layouts), Some(tag)) => UnionFieldLayouts::Found {
            field_layouts: union_layouts[tag as usize],
            tag,
        },
        (UnionLayout::NonNullableUnwrapped(union_layouts), None) => {
            // This union has just a single tag. So the tag is 0.
            UnionFieldLayouts::Found {
                field_layouts: union_layouts,
                tag: 0,
            }
        }
        (
            UnionLayout::NullableWrapped {
                nullable_id,
                other_tags,
            },
            Some(tag),
        ) => {
            match Ord::cmp(&tag, &nullable_id) {
                // tag is less than nullable_id, so the index is the same as the tag.
                cmp::Ordering::Less => UnionFieldLayouts::Found {
                    field_layouts: other_tags[tag as usize],
                    tag,
                },
                // tag and nullable_id are equal, so the union is null.
                cmp::Ordering::Equal => UnionFieldLayouts::Null,
                // tag is greater than nullable_id, so the index is the tag - 1 (as the nullable tag is in between).
                cmp::Ordering::Greater => UnionFieldLayouts::Found {
                    field_layouts: other_tags[(tag as usize) - 1],
                    tag,
                },
            }
        }
        (
            UnionLayout::NullableUnwrapped {
                nullable_id,
                other_fields,
            },
            Some(tag),
        ) => {
            if tag == (nullable_id as u16) {
                UnionFieldLayouts::Null
            } else {
                UnionFieldLayouts::Found {
                    field_layouts: other_fields,
                    tag,
                }
            }
        }
        (_, _) => UnionFieldLayouts::Unknown,
    }
}

/**
Branch on the uniqueness of a symbol.
Using a joinpoint with the continuation as the body.
*/
fn branch_uniqueness<'a, 'i, F1, F2>(
    arena: &'a Bump,
    ident_ids: &'i mut IdentIds,
    layout_interner: &'i mut STLayoutInterner<'a>,
    environment: &DropSpecializationEnvironment<'a>,
    symbol: Symbol,
    unique: F1,
    not_unique: F2,
    continutation: &'a Stmt<'a>,
) -> &'a Stmt<'a>
where
    F1: FnOnce(&mut STLayoutInterner<'a>, &mut IdentIds, &'a Stmt<'a>) -> &'a Stmt<'a>,
    F2: FnOnce(&mut STLayoutInterner<'a>, &mut IdentIds, &'a Stmt<'a>) -> &'a Stmt<'a>,
{
    match continutation {
        // The continuation is a single stmt. So we can insert it inline and skip creating a joinpoint.
        Stmt::Ret(_) | Stmt::Jump(_, _) => {
            let u = unique(layout_interner, ident_ids, continutation);
            let n = not_unique(layout_interner, ident_ids, continutation);

            let switch = |unique_symbol| {
                arena.alloc(Stmt::Switch {
                    cond_symbol: unique_symbol,
                    cond_layout: Layout::BOOL,
                    branches: &*arena.alloc([(1, BranchInfo::None, u.clone())]),
                    default_branch: (BranchInfo::None, n),
                    ret_layout: environment.layout,
                })
            };

            unique_symbol(arena, ident_ids, environment, symbol, switch)
        }
        // We put the continuation in a joinpoint. To prevent duplicating the content.
        _ => {
            let join_id = JoinPointId(environment.create_symbol(ident_ids, "uniqueness_join"));

            let jump = arena.alloc(Stmt::Jump(join_id, arena.alloc([])));

            let u = unique(layout_interner, ident_ids, jump);
            let n = not_unique(layout_interner, ident_ids, jump);

            let switch = |unique_symbol| {
                arena.alloc(Stmt::Switch {
                    cond_symbol: unique_symbol,
                    cond_layout: Layout::BOOL,
                    branches: &*arena.alloc([(1, BranchInfo::None, u.clone())]),
                    default_branch: (BranchInfo::None, n),
                    ret_layout: environment.layout,
                })
            };

            let unique = unique_symbol(arena, ident_ids, environment, symbol, switch);

            arena.alloc(Stmt::Join {
                id: join_id,
                parameters: arena.alloc([]),
                body: continutation,
                remainder: unique,
            })
        }
    }
}

fn unique_symbol<'a, 'i>(
    arena: &'a Bump,
    ident_ids: &'i mut IdentIds,
    environment: &DropSpecializationEnvironment<'a>,
    symbol: Symbol,
    continuation: impl FnOnce(Symbol) -> &'a mut Stmt<'a>,
) -> &'a Stmt<'a> {
    let is_unique = environment.create_symbol(ident_ids, "is_unique");

    arena.alloc(Stmt::Let(
        is_unique,
        Expr::Call(Call {
            call_type: CallType::LowLevel {
                op: LowLevel::RefCountIsUnique,
                update_mode: UpdateModeId::BACKEND_DUMMY,
            },
            arguments: arena.alloc([symbol]),
        }),
        Layout::BOOL,
        continuation(is_unique),
    ))
}

enum UnionFieldLayouts<'a> {
    Found {
        field_layouts: &'a [InLayout<'a>],
        tag: Tag,
    },
    Unknown,
    Null,
}

type Index = u64;

type Parent = Symbol;

type Child = Symbol;

type Tag = u16;

#[derive(Clone)]
struct DropSpecializationEnvironment<'a> {
    arena: &'a Bump,
    home: ModuleId,
    layout: InLayout<'a>,
    target_info: TargetInfo,

    symbol_layouts: MutMap<Symbol, InLayout<'a>>,

    // Keeps track of which parent symbol is indexed by which child symbol for structs
    struct_children: MutMap<Parent, Vec<'a, (Child, Index)>>,

    // Keeps track of which parent symbol is indexed by which child symbol for unions
    union_children: MutMap<Parent, Vec<'a, (Child, Tag, Index)>>,

    // Keeps track of which parent symbol is indexed by which child symbol for boxes
    box_children: MutMap<Parent, Vec<'a, Child>>,

    // Keeps track of which parent symbol is indexed by which child symbol for lists
    list_children: MutMap<Parent, Vec<'a, (Child, Index)>>,

    // Keeps track of all incremented symbols.
    incremented_symbols: MutMap<Symbol, u64>,

    // Map containing the current known tag of a layout.
    symbol_tag: MutMap<Symbol, Tag>,

    // Map containing the current known index value of a symbol.
    symbol_index: MutMap<Symbol, Index>,

    // Map containing the current known length of a list.
    list_length: MutMap<Symbol, u64>,
}

impl<'a> DropSpecializationEnvironment<'a> {
    fn new(arena: &'a Bump, home: ModuleId, layout: InLayout<'a>, target_info: TargetInfo) -> Self {
        Self {
            arena,
            home,
            layout,
            target_info,
            symbol_layouts: MutMap::default(),
            struct_children: MutMap::default(),
            union_children: MutMap::default(),
            box_children: MutMap::default(),
            list_children: MutMap::default(),
            incremented_symbols: MutMap::default(),
            symbol_tag: MutMap::default(),
            symbol_index: MutMap::default(),
            list_length: MutMap::default(),
        }
    }

    fn clone_without_incremented(&self) -> Self {
        Self {
            arena: self.arena,
            home: self.home,
            layout: self.layout,
            target_info: self.target_info,
            symbol_layouts: self.symbol_layouts.clone(),
            struct_children: self.struct_children.clone(),
            union_children: self.union_children.clone(),
            box_children: self.box_children.clone(),
            list_children: self.list_children.clone(),
            incremented_symbols: MutMap::default(),
            symbol_tag: self.symbol_tag.clone(),
            symbol_index: self.symbol_index.clone(),
            list_length: self.list_length.clone(),
        }
    }

    fn create_symbol<'i>(&self, ident_ids: &'i mut IdentIds, debug_name: &str) -> Symbol {
        let ident_id = ident_ids.add_str(debug_name);
        Symbol::new(self.home, ident_id)
    }

    fn add_symbol_layout(&mut self, symbol: Symbol, layout: InLayout<'a>) {
        self.symbol_layouts.insert(symbol, layout);
    }

    fn get_symbol_layout(&self, symbol: &Symbol) -> &InLayout<'a> {
        self.symbol_layouts
            .get(symbol)
            .expect("All symbol layouts should be known.")
    }

    fn add_struct_child(&mut self, parent: Parent, child: Child, index: Index) {
        self.struct_children
            .entry(parent)
            .or_insert_with(|| Vec::new_in(self.arena))
            .push((child, index));
    }

    fn add_union_child(&mut self, parent: Parent, child: Child, tag: u16, index: Index) {
        self.union_children
            .entry(parent)
            .or_insert_with(|| Vec::new_in(self.arena))
            .push((child, tag, index));
    }

    fn add_box_child(&mut self, parent: Parent, child: Child) {
        self.box_children
            .entry(parent)
            .or_insert_with(|| Vec::new_in(self.arena))
            .push(child);
    }

    fn add_list_child(&mut self, parent: Parent, child: Child, index: &Symbol) {
        if let Some(index) = self.symbol_index.get(index) {
            self.list_children
                .entry(parent)
                .or_insert_with(|| Vec::new_in(self.arena))
                .push((child, *index));
        }
    }

    fn get_children(&self, parent: &Parent) -> Vec<'a, Symbol> {
        let mut res = Vec::new_in(self.arena);

        if let Some(children) = self.struct_children.get(parent) {
            res.extend(children.iter().map(|(child, _)| child));
        }

        if let Some(children) = self.union_children.get(parent) {
            res.extend(children.iter().map(|(child, _, _)| child));
        }

        if let Some(children) = self.box_children.get(parent) {
            res.extend(children.iter());
        }

        if let Some(children) = self.list_children.get(parent) {
            res.extend(children.iter().map(|(child, _)| child));
        }

        res
    }

    /**
    Add a symbol for every increment performed.
     */
    fn add_incremented(&mut self, symbol: Symbol, count: u64) {
        self.incremented_symbols
            .entry(symbol)
            .and_modify(|c| *c += count)
            .or_insert(count);
    }

    fn any_incremented(&self, symbol: &Symbol) -> bool {
        self.incremented_symbols.contains_key(symbol)
    }

    /**
    Return the amount of times a symbol still has to be incremented.
    Accounting for later consumtion and removal of the increment.
    */
    fn get_incremented(&mut self, symbol: &Symbol) -> u64 {
        self.incremented_symbols.remove(symbol).unwrap_or(0)
    }

    fn pop_incremented(&mut self, symbol: &Symbol) -> bool {
        match self.incremented_symbols.get_mut(symbol) {
            Some(1) => {
                self.incremented_symbols.remove(symbol);
                true
            }
            Some(c) => {
                *c -= 1;
                true
            }
            None => false,
        }
    }

    // TODO assert that a parent is only inlined once / assert max single dec per parent.
}
