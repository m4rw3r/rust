// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use llvm::{AvailableExternallyLinkage, InternalLinkage, SetLinkage};
use metadata::csearch;
use metadata::inline::InlinedItem;
use middle::astencode;
use middle::subst::Substs;
use trans::base::{push_ctxt, trans_item, get_item_val, trans_fn};
use trans::common::*;

use syntax::ast;
use syntax::ast_util::local_def;

fn instantiate_inline(ccx: &CrateContext, fn_id: ast::DefId)
    -> Option<ast::DefId> {
    debug!("instantiate_inline({:?})", fn_id);
    let _icx = push_ctxt("instantiate_inline");

    match ccx.external().borrow().get(&fn_id) {
        Some(&Some(node_id)) => {
            // Already inline
            debug!("instantiate_inline({}): already inline as node id {}",
                   ccx.tcx().item_path_str(fn_id), node_id);
            return Some(local_def(node_id));
        }
        Some(&None) => {
            return None; // Not inlinable
        }
        None => {
            // Not seen yet
        }
    }

    let csearch_result =
        csearch::maybe_get_item_ast(
            ccx.tcx(), fn_id,
            Box::new(|a,b,c,d| astencode::decode_inlined_item(a, b, c, d)));

    let inline_id = match csearch_result {
        csearch::FoundAst::NotFound => {
            ccx.external().borrow_mut().insert(fn_id, None);
            return None;
        }
        csearch::FoundAst::Found(&InlinedItem::Item(ref item)) => {
            ccx.external().borrow_mut().insert(fn_id, Some(item.id));
            ccx.external_srcs().borrow_mut().insert(item.id, fn_id);

            ccx.stats().n_inlines.set(ccx.stats().n_inlines.get() + 1);
            trans_item(ccx, item);

            let linkage = match item.node {
                ast::ItemFn(_, _, _, _, ref generics, _) => {
                    if generics.is_type_parameterized() {
                        // Generics have no symbol, so they can't be given any
                        // linkage.
                        None
                    } else {
                        if ccx.sess().opts.cg.codegen_units == 1 {
                            // We could use AvailableExternallyLinkage here,
                            // but InternalLinkage allows LLVM to optimize more
                            // aggressively (at the cost of sometimes
                            // duplicating code).
                            Some(InternalLinkage)
                        } else {
                            // With multiple compilation units, duplicated code
                            // is more of a problem.  Also, `codegen_units > 1`
                            // means the user is okay with losing some
                            // performance.
                            Some(AvailableExternallyLinkage)
                        }
                    }
                }
                ast::ItemConst(..) => None,
                _ => unreachable!(),
            };

            match linkage {
                Some(linkage) => {
                    let g = get_item_val(ccx, item.id);
                    SetLinkage(g, linkage);
                }
                None => {}
            }

            item.id
        }
        csearch::FoundAst::Found(&InlinedItem::Foreign(ref item)) => {
            ccx.external().borrow_mut().insert(fn_id, Some(item.id));
            ccx.external_srcs().borrow_mut().insert(item.id, fn_id);
            item.id
        }
        csearch::FoundAst::FoundParent(parent_id, &InlinedItem::Item(ref item)) => {
            ccx.external().borrow_mut().insert(parent_id, Some(item.id));
            ccx.external_srcs().borrow_mut().insert(item.id, parent_id);

            let mut my_id = 0;
            match item.node {
                ast::ItemEnum(ref ast_def, _) => {
                    let ast_vs = &ast_def.variants;
                    let ty_vs = &ccx.tcx().lookup_adt_def(parent_id).variants;
                    assert_eq!(ast_vs.len(), ty_vs.len());
                    for (ast_v, ty_v) in ast_vs.iter().zip(ty_vs.iter()) {
                        if ty_v.did == fn_id { my_id = ast_v.node.id; }
                        ccx.external().borrow_mut().insert(ty_v.did, Some(ast_v.node.id));
                    }
                }
                ast::ItemStruct(ref struct_def, _) => {
                    match struct_def.ctor_id {
                        None => ccx.sess().bug("instantiate_inline: called on a \
                                                non-tuple struct"),
                        Some(ctor_id) => {
                            ccx.external().borrow_mut().insert(fn_id, Some(ctor_id));
                            my_id = ctor_id;
                        }
                    }
                }
                _ => ccx.sess().bug("instantiate_inline: item has a \
                                 non-enum, non-struct parent")
            }
            trans_item(ccx, &**item);
            my_id
        }
        csearch::FoundAst::FoundParent(_, _) => {
            ccx.sess().bug("maybe_get_item_ast returned a FoundParent \
                            with a non-item parent");
        }
        csearch::FoundAst::Found(&InlinedItem::TraitItem(_, ref trait_item)) => {
            ccx.external().borrow_mut().insert(fn_id, Some(trait_item.id));
            ccx.external_srcs().borrow_mut().insert(trait_item.id, fn_id);

            ccx.stats().n_inlines.set(ccx.stats().n_inlines.get() + 1);

            // Associated consts already have to be evaluated in `typeck`, so
            // the logic to do that already exists in `middle`. In order to
            // reuse that code, it needs to be able to look up the traits for
            // inlined items.
            let ty_trait_item = ccx.tcx().impl_or_trait_item(fn_id).clone();
            ccx.tcx().impl_or_trait_items.borrow_mut()
                     .insert(local_def(trait_item.id), ty_trait_item);

            // If this is a default method, we can't look up the
            // impl type. But we aren't going to translate anyways, so
            // don't.
            trait_item.id
        }
        csearch::FoundAst::Found(&InlinedItem::ImplItem(impl_did, ref impl_item)) => {
            ccx.external().borrow_mut().insert(fn_id, Some(impl_item.id));
            ccx.external_srcs().borrow_mut().insert(impl_item.id, fn_id);

            ccx.stats().n_inlines.set(ccx.stats().n_inlines.get() + 1);

            // Translate monomorphic impl methods immediately.
            if let ast::MethodImplItem(ref sig, ref body) = impl_item.node {
                let impl_tpt = ccx.tcx().lookup_item_type(impl_did);
                if impl_tpt.generics.types.is_empty() &&
                        sig.generics.ty_params.is_empty() {
                    let empty_substs = ccx.tcx().mk_substs(Substs::trans_empty());
                    let llfn = get_item_val(ccx, impl_item.id);
                    trans_fn(ccx,
                             &sig.decl,
                             body,
                             llfn,
                             empty_substs,
                             impl_item.id,
                             &[]);
                    // See linkage comments on items.
                    if ccx.sess().opts.cg.codegen_units == 1 {
                        SetLinkage(llfn, InternalLinkage);
                    } else {
                        SetLinkage(llfn, AvailableExternallyLinkage);
                    }
                }
            }

            impl_item.id
        }
    };

    Some(local_def(inline_id))
}

pub fn get_local_instance(ccx: &CrateContext, fn_id: ast::DefId)
    -> Option<ast::DefId> {
    if fn_id.krate == ast::LOCAL_CRATE {
        Some(fn_id)
    } else {
        instantiate_inline(ccx, fn_id)
    }
}

pub fn maybe_instantiate_inline(ccx: &CrateContext, fn_id: ast::DefId) -> ast::DefId {
    get_local_instance(ccx, fn_id).unwrap_or(fn_id)
}
