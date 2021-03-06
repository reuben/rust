/**
   Code that is useful in various trans modules.

*/

import core::{int, vec, str, uint, option, unsafe};
import vec::to_ptr;
import std::map::hashmap;
import option::some;
import syntax::ast;
import driver::session;
import session::session;
import middle::{resolve, ty};
import back::{link, abi, upcall};
import util::common::*;
import syntax::codemap::span;
import lib::llvm::{llvm, target_data, type_names, associate_type,
                   name_has_type};
import lib::llvm::llvm::{ModuleRef, ValueRef, TypeRef, BasicBlockRef};
import lib::llvm::{True, False, Bool};
import metadata::{csearch};

// FIXME: These should probably be pulled in here too.
import trans::{type_of_fn, drop_ty};

type namegen = fn@(str) -> str;
fn new_namegen() -> namegen {
    let i = @mutable 0;
    ret fn@(prefix: str) -> str { *i += 1; prefix + int::str(*i) };
}

type derived_tydesc_info = {lltydesc: ValueRef, escapes: bool};

tag tydesc_kind {
    tk_static; // Static (monomorphic) type descriptor.
    tk_param; // Type parameter.
    tk_derived; // Derived from a typaram or another derived tydesc.
}

type tydesc_info =
    {ty: ty::t,
     tydesc: ValueRef,
     size: ValueRef,
     align: ValueRef,
     mutable take_glue: option::t<ValueRef>,
     mutable drop_glue: option::t<ValueRef>,
     mutable free_glue: option::t<ValueRef>,
     mutable cmp_glue: option::t<ValueRef>,
     ty_params: [uint]};

/*
 * A note on nomenclature of linking: "upcall", "extern" and "native".
 *
 * An "extern" is an LLVM symbol we wind up emitting an undefined external
 * reference to. This means "we don't have the thing in this compilation unit,
 * please make sure you link it in at runtime". This could be a reference to
 * C code found in a C library, or rust code found in a rust crate.
 *
 * A "native" is an extern that references C code. Called with cdecl.
 *
 * An upcall is a native call generated by the compiler (not corresponding to
 * any user-written call in the code) into librustrt, to perform some helper
 * task such as bringing a task to life, allocating memory, etc.
 *
 */
type stats =
    {mutable n_static_tydescs: uint,
     mutable n_derived_tydescs: uint,
     mutable n_glues_created: uint,
     mutable n_null_glues: uint,
     mutable n_real_glues: uint,
     fn_times: @mutable [{ident: str, time: int}]};

resource BuilderRef_res(B: llvm::BuilderRef) { llvm::LLVMDisposeBuilder(B); }

// Crate context.  Every crate we compile has one of these.
type crate_ctxt =
    // A mapping from the def_id of each item in this crate to the address
    // of the first instruction of the item's definition in the executable
    // we're generating.
    // TODO: hashmap<tup(tag_id,subtys), @tag_info>
    {sess: session::session,
     llmod: ModuleRef,
     td: target_data,
     tn: type_names,
     externs: hashmap<str, ValueRef>,
     intrinsics: hashmap<str, ValueRef>,
     item_ids: hashmap<ast::node_id, ValueRef>,
     ast_map: ast_map::map,
     exp_map: resolve::exp_map,
     item_symbols: hashmap<ast::node_id, str>,
     mutable main_fn: option::t<ValueRef>,
     link_meta: link::link_meta,
     tag_sizes: hashmap<ty::t, uint>,
     discrims: hashmap<ast::def_id, ValueRef>,
     discrim_symbols: hashmap<ast::node_id, str>,
     consts: hashmap<ast::node_id, ValueRef>,
     tydescs: hashmap<ty::t, @tydesc_info>,
     dicts: hashmap<dict_id, ValueRef>,
     module_data: hashmap<str, ValueRef>,
     lltypes: hashmap<ty::t, TypeRef>,
     names: namegen,
     sha: std::sha1::sha1,
     type_sha1s: hashmap<ty::t, str>,
     type_short_names: hashmap<ty::t, str>,
     tcx: ty::ctxt,
     mut_map: mut::mut_map,
     copy_map: alias::copy_map,
     last_uses: last_use::last_uses,
     method_map: typeck::method_map,
     dict_map: typeck::dict_map,
     stats: stats,
     upcalls: @upcall::upcalls,
     tydesc_type: TypeRef,
     int_type: TypeRef,
     float_type: TypeRef,
     task_type: TypeRef,
     opaque_vec_type: TypeRef,
     builder: BuilderRef_res,
     shape_cx: shape::ctxt,
     gc_cx: gc::ctxt,
     crate_map: ValueRef,
     dbg_cx: option::t<@debuginfo::debug_ctxt>};

type local_ctxt =
    {path: [str],
     module_path: [str],
     ccx: @crate_ctxt};

// Types used for llself.
type val_self_pair = {v: ValueRef, t: ty::t};

tag local_val { local_mem(ValueRef); local_imm(ValueRef); }

type fn_ty_param = {desc: ValueRef, dicts: option::t<[ValueRef]>};

// Function context.  Every LLVM function we create will have one of
// these.
type fn_ctxt =
    // The ValueRef returned from a call to llvm::LLVMAddFunction; the
    // address of the first instruction in the sequence of
    // instructions for this function that will go in the .text
    // section of the executable we're generating.

    // The three implicit arguments that arrive in the function we're
    // creating.  For instance, foo(int, int) is really foo(ret*,
    // task*, env*, int, int).  These are also available via
    // llvm::LLVMGetParam(llfn, uint) where uint = 1, 2, 0
    // respectively, but we unpack them into these fields for
    // convenience.

    // Points to the current task.

    // Points to the current environment (bindings of variables to
    // values), if this is a regular function

    // Points to where the return value of this function should end
    // up.

    // The next three elements: "hoisted basic blocks" containing
    // administrative activities that have to happen in only one place in
    // the function, due to LLVM's quirks.

    // A block for all the function's static allocas, so that LLVM
    // will coalesce them into a single alloca call.

    // A block containing code that copies incoming arguments to space
    // already allocated by code in one of the llallocas blocks.
    // (LLVM requires that arguments be copied to local allocas before
    // allowing most any operation to be performed on them.)

    // The first block containing derived tydescs received from the
    // runtime.  See description of derived_tydescs, below.

    // The last block of the llderivedtydescs group.

    // A block for all of the dynamically sized allocas.  This must be
    // after llderivedtydescs, because these sometimes depend on
    // information computed from derived tydescs.

    // The token used to clear the dynamic allocas at the end of this frame.

    // The 'self' value currently in use in this function, if there
    // is one.

    // If this function is actually a iter, a block containing the
    // code called whenever the iter calls 'put'.

    // The next four items: hash tables mapping from AST def_ids to
    // LLVM-stuff-in-the-frame.

    // Maps arguments to allocas created for them in llallocas.

    // Maps the def_ids for local variables to the allocas created for
    // them in llallocas.

    // The same as above, but for variables accessed via the frame
    // pointer we pass into an iter, for access to the static
    // environment of the iter-calling frame.

    // For convenience, a vector of the incoming tydescs for each of
    // this functions type parameters, fetched via llvm::LLVMGetParam.
    // For example, for a function foo::<A, B, C>(), lltydescs contains
    // the ValueRefs for the tydescs for A, B, and C.

    // Derived tydescs are tydescs created at runtime, for types that
    // involve type parameters inside type constructors.  For example,
    // suppose a function parameterized by T creates a vector of type
    // [T].  The function doesn't know what T is until runtime, and
    // the function's caller knows T but doesn't know that a vector is
    // involved.  So a tydesc for [T] can't be created until runtime,
    // when information about both "[T]" and "T" are available.  When
    // such a tydesc is created, we cache it in the derived_tydescs
    // table for the next time that such a tydesc is needed.

    // The node_id of the function, or -1 if it doesn't correspond to
    // a user-defined function.

    // The source span where this function comes from, for error
    // reporting.

    // This function's enclosing local context.
    {llfn: ValueRef,
     llenv: ValueRef,
     llretptr: ValueRef,
     mutable llstaticallocas: BasicBlockRef,
     mutable llloadenv: BasicBlockRef,
     mutable llderivedtydescs_first: BasicBlockRef,
     mutable llderivedtydescs: BasicBlockRef,
     mutable lldynamicallocas: BasicBlockRef,
     mutable llreturn: BasicBlockRef,
     mutable llobstacktoken: option::t<ValueRef>,
     mutable llself: option::t<val_self_pair>,
     llargs: hashmap<ast::node_id, local_val>,
     lllocals: hashmap<ast::node_id, local_val>,
     llupvars: hashmap<ast::node_id, ValueRef>,
     mutable lltyparams: [fn_ty_param],
     derived_tydescs: hashmap<ty::t, derived_tydesc_info>,
     id: ast::node_id,
     ret_style: ast::ret_style,
     sp: span,
     lcx: @local_ctxt};

tag cleanup {
    clean(fn@(@block_ctxt) -> @block_ctxt);
    clean_temp(ValueRef, fn@(@block_ctxt) -> @block_ctxt);
}

fn add_clean(cx: @block_ctxt, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(bcx_tcx(cx), ty) { ret; }
    let scope_cx = find_scope_cx(cx);
    scope_cx.cleanups += [clean(bind drop_ty(_, val, ty))];
    scope_cx.lpad_dirty = true;
}
fn add_clean_temp(cx: @block_ctxt, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(bcx_tcx(cx), ty) { ret; }
    fn do_drop(bcx: @block_ctxt, val: ValueRef, ty: ty::t) ->
       @block_ctxt {
        if ty::type_is_immediate(bcx_tcx(bcx), ty) {
            ret trans::drop_ty_immediate(bcx, val, ty);
        } else {
            ret drop_ty(bcx, val, ty);
        }
    }
    let scope_cx = find_scope_cx(cx);
    scope_cx.cleanups +=
        [clean_temp(val, bind do_drop(_, val, ty))];
    scope_cx.lpad_dirty = true;
}
fn add_clean_temp_mem(cx: @block_ctxt, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(bcx_tcx(cx), ty) { ret; }
    let scope_cx = find_scope_cx(cx);
    scope_cx.cleanups += [clean_temp(val, bind drop_ty(_, val, ty))];
    scope_cx.lpad_dirty = true;
}
fn add_clean_free(cx: @block_ctxt, ptr: ValueRef, shared: bool) {
    let scope_cx = find_scope_cx(cx);
    let free_fn = if shared { bind trans::trans_shared_free(_, ptr) }
                  else { bind trans::trans_free_if_not_gc(_, ptr) };
    scope_cx.cleanups += [clean_temp(ptr, free_fn)];
    scope_cx.lpad_dirty = true;
}

// Note that this only works for temporaries. We should, at some point, move
// to a system where we can also cancel the cleanup on local variables, but
// this will be more involved. For now, we simply zero out the local, and the
// drop glue checks whether it is zero.
fn revoke_clean(cx: @block_ctxt, val: ValueRef) {
    let sc_cx = find_scope_cx(cx);
    let found = -1;
    let i = 0;
    for c: cleanup in sc_cx.cleanups {
        alt c {
          clean_temp(v, _) {
            if v as uint == val as uint { found = i; break; }
          }
          _ { }
        }
        i += 1;
    }
    // The value does not have a cleanup associated with it.
    if found == -1 { ret; }
    // We found the cleanup and remove it
    sc_cx.cleanups =
        vec::slice(sc_cx.cleanups, 0u, found as uint) +
            vec::slice(sc_cx.cleanups, (found as uint) + 1u,
                            vec::len(sc_cx.cleanups));
    sc_cx.lpad_dirty = true;
    ret;
}

fn get_res_dtor(ccx: @crate_ctxt, sp: span, did: ast::def_id, inner_t: ty::t)
   -> ValueRef {
    if did.crate == ast::local_crate {
        alt ccx.item_ids.find(did.node) {
          some(x) { ret x; }
          _ { ccx.tcx.sess.bug("get_res_dtor: can't find resource dtor!"); }
        }
    }

    let param_bounds = ty::lookup_item_type(ccx.tcx, did).bounds;
    let nil_res = ty::mk_nil(ccx.tcx);
    // FIXME: Silly check -- mk_nil should have a postcondition
    check non_ty_var(ccx, nil_res);
    let f_t = type_of_fn(ccx, sp,
                         [{mode: ast::by_ref, ty: inner_t}],
                         nil_res, *param_bounds);
    ret trans::get_extern_const(ccx.externs, ccx.llmod,
                                csearch::get_symbol(ccx.sess.cstore,
                                                    did), f_t);
}

tag block_kind {


    // A scope block is a basic block created by translating a block { ... }
    // the the source language.  Since these blocks create variable scope, any
    // variables created in them that are still live at the end of the block
    // must be dropped and cleaned up when the block ends.
    SCOPE_BLOCK;


    // A basic block created from the body of a loop.  Contains pointers to
    // which block to jump to in the case of "continue" or "break", with the
    // "continue" block optional, because "while" and "do while" don't support
    // "continue" (TODO: is this intentional?)
    LOOP_SCOPE_BLOCK(option::t<@block_ctxt>, @block_ctxt);


    // A non-scope block is a basic block created as a translation artifact
    // from translating code that expresses conditional logic rather than by
    // explicit { ... } block structure in the source language.  It's called a
    // non-scope block because it doesn't introduce a new variable scope.
    NON_SCOPE_BLOCK;
}


// Basic block context.  We create a block context for each basic block
// (single-entry, single-exit sequence of instructions) we generate from Rust
// code.  Each basic block we generate is attached to a function, typically
// with many basic blocks per function.  All the basic blocks attached to a
// function are organized as a directed graph.
type block_ctxt =
    // The BasicBlockRef returned from a call to
    // llvm::LLVMAppendBasicBlock(llfn, name), which adds a basic
    // block to the function pointed to by llfn.  We insert
    // instructions into that block by way of this block context.
    // The block pointing to this one in the function's digraph.
    // The 'kind' of basic block this is.
    // A list of functions that run at the end of translating this
    // block, cleaning up any variables that were introduced in the
    // block and need to go out of scope at the end of it.
    // The source span where this block comes from, for error
    // reporting. FIXME this is not currently reliable
    // The function context for the function to which this block is
    // attached.
    {llbb: BasicBlockRef,
     mutable terminated: bool,
     mutable unreachable: bool,
     parent: block_parent,
     kind: block_kind,
     mutable cleanups: [cleanup],
     mutable lpad_dirty: bool,
     mutable lpad: option::t<BasicBlockRef>,
     sp: span,
     fcx: @fn_ctxt};

// FIXME: we should be able to use option::t<@block_parent> here but
// the infinite-tag check in rustboot gets upset.
tag block_parent { parent_none; parent_some(@block_ctxt); }

type result = {bcx: @block_ctxt, val: ValueRef};
type result_t = {bcx: @block_ctxt, val: ValueRef, ty: ty::t};

fn extend_path(cx: @local_ctxt, name: str) -> @local_ctxt {
    ret @{path: cx.path + [name] with *cx};
}

fn rslt(bcx: @block_ctxt, val: ValueRef) -> result {
    {bcx: bcx, val: val}
}

fn ty_str(tn: type_names, t: TypeRef) -> str {
    ret lib::llvm::type_to_str(tn, t);
}

fn val_ty(&&v: ValueRef) -> TypeRef { ret llvm::LLVMTypeOf(v); }

fn val_str(tn: type_names, v: ValueRef) -> str { ret ty_str(tn, val_ty(v)); }

// Returns the nth element of the given LLVM structure type.
fn struct_elt(llstructty: TypeRef, n: uint) -> TypeRef unsafe {
    let elt_count = llvm::LLVMCountStructElementTypes(llstructty);
    assert (n < elt_count);
    let elt_tys = vec::init_elt(T_nil(), elt_count);
    llvm::LLVMGetStructElementTypes(llstructty, to_ptr(elt_tys));
    ret llvm::LLVMGetElementType(elt_tys[n]);
}

fn find_scope_cx(cx: @block_ctxt) -> @block_ctxt {
    if cx.kind != NON_SCOPE_BLOCK { ret cx; }
    alt cx.parent {
      parent_some(b) { ret find_scope_cx(b); }
      parent_none. {
        cx.fcx.lcx.ccx.sess.bug("trans::find_scope_cx() " +
                                    "called on parentless block_ctxt");
      }
    }
}

// Accessors
// TODO: When we have overloading, simplify these names!

pure fn bcx_tcx(bcx: @block_ctxt) -> ty::ctxt { ret bcx.fcx.lcx.ccx.tcx; }
pure fn bcx_ccx(bcx: @block_ctxt) -> @crate_ctxt { ret bcx.fcx.lcx.ccx; }
pure fn bcx_lcx(bcx: @block_ctxt) -> @local_ctxt { ret bcx.fcx.lcx; }
pure fn bcx_fcx(bcx: @block_ctxt) -> @fn_ctxt { ret bcx.fcx; }
pure fn fcx_ccx(fcx: @fn_ctxt) -> @crate_ctxt { ret fcx.lcx.ccx; }
pure fn fcx_tcx(fcx: @fn_ctxt) -> ty::ctxt { ret fcx.lcx.ccx.tcx; }
pure fn lcx_ccx(lcx: @local_ctxt) -> @crate_ctxt { ret lcx.ccx; }
pure fn ccx_tcx(ccx: @crate_ctxt) -> ty::ctxt { ret ccx.tcx; }

// LLVM type constructors.
fn T_void() -> TypeRef {
    // Note: For the time being llvm is kinda busted here, it has the notion
    // of a 'void' type that can only occur as part of the signature of a
    // function, but no general unit type of 0-sized value. This is, afaict,
    // vestigial from its C heritage, and we'll be attempting to submit a
    // patch upstream to fix it. In the mean time we only model function
    // outputs (Rust functions and C functions) using T_void, and model the
    // Rust general purpose nil type you can construct as 1-bit (always
    // zero). This makes the result incorrect for now -- things like a tuple
    // of 10 nil values will have 10-bit size -- but it doesn't seem like we
    // have any other options until it's fixed upstream.

    ret llvm::LLVMVoidType();
}

fn T_nil() -> TypeRef {
    // NB: See above in T_void().

    ret llvm::LLVMInt1Type();
}

fn T_metadata() -> TypeRef { ret llvm::LLVMMetadataType(); }

fn T_i1() -> TypeRef { ret llvm::LLVMInt1Type(); }

fn T_i8() -> TypeRef { ret llvm::LLVMInt8Type(); }

fn T_i16() -> TypeRef { ret llvm::LLVMInt16Type(); }

fn T_i32() -> TypeRef { ret llvm::LLVMInt32Type(); }

fn T_i64() -> TypeRef { ret llvm::LLVMInt64Type(); }

fn T_f32() -> TypeRef { ret llvm::LLVMFloatType(); }

fn T_f64() -> TypeRef { ret llvm::LLVMDoubleType(); }

fn T_bool() -> TypeRef { ret T_i1(); }

fn T_int(targ_cfg: @session::config) -> TypeRef {
    ret alt targ_cfg.arch {
      session::arch_x86. { T_i32() }
      session::arch_x86_64. { T_i64() }
      session::arch_arm. { T_i32() }
    };
}

fn T_int_ty(cx: @crate_ctxt, t: ast::int_ty) -> TypeRef {
    alt t {
      ast::ty_i. { cx.int_type }
      ast::ty_char. { T_char() }
      ast::ty_i8. { T_i8() }
      ast::ty_i16. { T_i16() }
      ast::ty_i32. { T_i32() }
      ast::ty_i64. { T_i64() }
    }
}

fn T_uint_ty(cx: @crate_ctxt, t: ast::uint_ty) -> TypeRef {
    alt t {
      ast::ty_u. { cx.int_type }
      ast::ty_u8. { T_i8() }
      ast::ty_u16. { T_i16() }
      ast::ty_u32. { T_i32() }
      ast::ty_u64. { T_i64() }
    }
}

fn T_float_ty(cx: @crate_ctxt, t: ast::float_ty) -> TypeRef {
    alt t {
      ast::ty_f. { cx.float_type }
      ast::ty_f32. { T_f32() }
      ast::ty_f64. { T_f64() }
    }
}

fn T_float(targ_cfg: @session::config) -> TypeRef {
    ret alt targ_cfg.arch {
      session::arch_x86. { T_f64() }
      session::arch_x86_64. { T_f64() }
      session::arch_arm. { T_f64() }
    };
}

fn T_char() -> TypeRef { ret T_i32(); }

fn T_size_t(targ_cfg: @session::config) -> TypeRef {
    ret T_int(targ_cfg);
}

fn T_fn(inputs: [TypeRef], output: TypeRef) -> TypeRef unsafe {
    ret llvm::LLVMFunctionType(output, to_ptr(inputs),
                               vec::len::<TypeRef>(inputs), False);
}

fn T_fn_pair(cx: @crate_ctxt, tfn: TypeRef) -> TypeRef {
    ret T_struct([T_ptr(tfn), T_opaque_cbox_ptr(cx)]);
}

fn T_ptr(t: TypeRef) -> TypeRef { ret llvm::LLVMPointerType(t, 0u); }

fn T_struct(elts: [TypeRef]) -> TypeRef unsafe {
    ret llvm::LLVMStructType(to_ptr(elts), vec::len(elts), False);
}

fn T_named_struct(name: str) -> TypeRef {
    let c = llvm::LLVMGetGlobalContext();
    ret str::as_buf(name, {|buf| llvm::LLVMStructCreateNamed(c, buf) });
}

fn set_struct_body(t: TypeRef, elts: [TypeRef]) unsafe {
    llvm::LLVMStructSetBody(t, to_ptr(elts), vec::len(elts), False);
}

fn T_empty_struct() -> TypeRef { ret T_struct([]); }

// A dict is, in reality, a vtable pointer followed by zero or more pointers
// to tydescs and other dicts that it closes over. But the types and number of
// those are rarely known to the code that needs to manipulate them, so they
// are described by this opaque type.
fn T_dict() -> TypeRef { T_array(T_ptr(T_i8()), 1u) }

fn T_task(targ_cfg: @session::config) -> TypeRef {
    let t = T_named_struct("task");

    // Refcount
    // Delegate pointer
    // Stack segment pointer
    // Runtime SP
    // Rust SP
    // GC chain


    // Domain pointer
    // Crate cache pointer

    let t_int = T_int(targ_cfg);
    let elems =
        [t_int, t_int, t_int, t_int,
         t_int, t_int, t_int, t_int];
    set_struct_body(t, elems);
    ret t;
}

fn T_tydesc_field(cx: @crate_ctxt, field: int) -> TypeRef unsafe {
    // Bit of a kludge: pick the fn typeref out of the tydesc..

    let tydesc_elts: [TypeRef] =
        vec::init_elt::<TypeRef>(T_nil(),
                                      abi::n_tydesc_fields as uint);
    llvm::LLVMGetStructElementTypes(cx.tydesc_type,
                                    to_ptr::<TypeRef>(tydesc_elts));
    let t = llvm::LLVMGetElementType(tydesc_elts[field]);
    ret t;
}

fn T_glue_fn(cx: @crate_ctxt) -> TypeRef {
    let s = "glue_fn";
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t = T_tydesc_field(cx, abi::tydesc_field_drop_glue);
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_cmp_glue_fn(cx: @crate_ctxt) -> TypeRef {
    let s = "cmp_glue_fn";
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t = T_tydesc_field(cx, abi::tydesc_field_cmp_glue);
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_tydesc(targ_cfg: @session::config) -> TypeRef {
    let tydesc = T_named_struct("tydesc");
    let tydescpp = T_ptr(T_ptr(tydesc));
    let pvoid = T_ptr(T_i8());
    let glue_fn_ty =
        T_ptr(T_fn([T_ptr(T_nil()), T_ptr(T_nil()), tydescpp,
                    pvoid], T_void()));
    let cmp_glue_fn_ty =
        T_ptr(T_fn([T_ptr(T_i1()), T_ptr(tydesc), tydescpp,
                    pvoid, pvoid, T_i8()], T_void()));

    let int_type = T_int(targ_cfg);
    let elems =
        [tydescpp, int_type, int_type,
         glue_fn_ty, glue_fn_ty, glue_fn_ty,
         T_ptr(T_i8()), glue_fn_ty, glue_fn_ty, glue_fn_ty, cmp_glue_fn_ty,
         T_ptr(T_i8()), T_ptr(T_i8()), int_type, int_type];
    set_struct_body(tydesc, elems);
    ret tydesc;
}

fn T_array(t: TypeRef, n: uint) -> TypeRef { ret llvm::LLVMArrayType(t, n); }

// Interior vector.
//
// TODO: Support user-defined vector sizes.
fn T_vec2(targ_cfg: @session::config, t: TypeRef) -> TypeRef {
    ret T_struct([T_int(targ_cfg), // fill
                  T_int(targ_cfg), // alloc
                  T_array(t, 0u)]); // elements
}

fn T_vec(ccx: @crate_ctxt, t: TypeRef) -> TypeRef {
    ret T_vec2(ccx.sess.targ_cfg, t);
}

// Note that the size of this one is in bytes.
fn T_opaque_vec(targ_cfg: @session::config) -> TypeRef {
    ret T_vec2(targ_cfg, T_i8());
}

fn T_box(cx: @crate_ctxt, t: TypeRef) -> TypeRef {
    ret T_struct([cx.int_type, t]);
}

fn T_port(cx: @crate_ctxt, _t: TypeRef) -> TypeRef {
    ret T_struct([cx.int_type]); // Refcount

}

fn T_chan(cx: @crate_ctxt, _t: TypeRef) -> TypeRef {
    ret T_struct([cx.int_type]); // Refcount

}

fn T_taskptr(cx: @crate_ctxt) -> TypeRef { ret T_ptr(cx.task_type); }


// This type must never be used directly; it must always be cast away.
fn T_typaram(tn: type_names) -> TypeRef {
    let s = "typaram";
    alt name_has_type(tn, s) { some(t) { ret t; } _ {} }
    let t = T_i8();
    associate_type(tn, s, t);
    ret t;
}

fn T_typaram_ptr(tn: type_names) -> TypeRef { ret T_ptr(T_typaram(tn)); }

fn T_opaque_cbox_ptr(cx: @crate_ctxt) -> TypeRef {
    let s = "*cbox";
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t = T_ptr(T_struct([cx.int_type,
                            T_ptr(cx.tydesc_type),
                            T_i8() /* represents closed over tydescs
                            and data go here; see trans_closure.rs*/
                           ]));
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_tag_variant(cx: @crate_ctxt) -> TypeRef {
    ret cx.int_type;
}

fn T_tag(cx: @crate_ctxt, size: uint) -> TypeRef {
    let s = "tag_" + uint::to_str(size, 10u);
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t =
        if size == 0u {
            T_struct([T_tag_variant(cx)])
        } else { T_struct([T_tag_variant(cx), T_array(T_i8(), size)]) };
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_opaque_tag(cx: @crate_ctxt) -> TypeRef {
    let s = "opaque_tag";
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t = T_struct([T_tag_variant(cx), T_i8()]);
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_opaque_tag_ptr(cx: @crate_ctxt) -> TypeRef {
    ret T_ptr(T_opaque_tag(cx));
}

fn T_captured_tydescs(cx: @crate_ctxt, n: uint) -> TypeRef {
    ret T_struct(vec::init_elt::<TypeRef>(T_ptr(cx.tydesc_type), n));
}

fn T_opaque_iface_ptr(cx: @crate_ctxt) -> TypeRef {
    let tdptr = T_ptr(cx.tydesc_type);
    T_ptr(T_box(cx, T_struct([tdptr, tdptr, T_i8()])))
}

fn T_opaque_port_ptr() -> TypeRef { ret T_ptr(T_i8()); }

fn T_opaque_chan_ptr() -> TypeRef { ret T_ptr(T_i8()); }


// LLVM constant constructors.
fn C_null(t: TypeRef) -> ValueRef { ret llvm::LLVMConstNull(t); }

fn C_integral(t: TypeRef, u: u64, sign_extend: Bool) -> ValueRef {
    let u_hi = (u >> 32u64) as uint;
    let u_lo = u as uint;
    ret llvm::LLVMRustConstInt(t, u_hi, u_lo, sign_extend);
}

fn C_floating(s: str, t: TypeRef) -> ValueRef {
    ret str::as_buf(s, {|buf| llvm::LLVMConstRealOfString(t, buf) });
}

fn C_nil() -> ValueRef {
    // NB: See comment above in T_void().

    ret C_integral(T_i1(), 0u64, False);
}

fn C_bool(b: bool) -> ValueRef {
    if b {
        ret C_integral(T_bool(), 1u64, False);
    } else { ret C_integral(T_bool(), 0u64, False); }
}

fn C_i32(i: i32) -> ValueRef {
    ret C_integral(T_i32(), i as u64, True);
}

fn C_i64(i: i64) -> ValueRef {
    ret C_integral(T_i64(), i as u64, True);
}

fn C_int(cx: @crate_ctxt, i: int) -> ValueRef {
    ret C_integral(cx.int_type, i as u64, True);
}

fn C_uint(cx: @crate_ctxt, i: uint) -> ValueRef {
    ret C_integral(cx.int_type, i as u64, False);
}

fn C_u8(i: uint) -> ValueRef { ret C_integral(T_i8(), i as u64, False); }


// This is a 'c-like' raw string, which differs from
// our boxed-and-length-annotated strings.
fn C_cstr(cx: @crate_ctxt, s: str) -> ValueRef {
    let sc =
        str::as_buf(s,
                    {|buf|
                        llvm::LLVMConstString(buf, str::byte_len(s), False)
                    });
    let g =
        str::as_buf(cx.names("str"),
                    {|buf| llvm::LLVMAddGlobal(cx.llmod, val_ty(sc), buf) });
    llvm::LLVMSetInitializer(g, sc);
    llvm::LLVMSetGlobalConstant(g, True);
    llvm::LLVMSetLinkage(g, lib::llvm::LLVMInternalLinkage as llvm::Linkage);
    ret g;
}

// Returns a Plain Old LLVM String:
fn C_postr(s: str) -> ValueRef {
    ret str::as_buf(s,
                    {|buf|
                        llvm::LLVMConstString(buf, str::byte_len(s), False)
                    });
}

fn C_zero_byte_arr(size: uint) -> ValueRef unsafe {
    let i = 0u;
    let elts: [ValueRef] = [];
    while i < size { elts += [C_u8(0u)]; i += 1u; }
    ret llvm::LLVMConstArray(T_i8(), vec::to_ptr(elts),
                             vec::len(elts));
}

fn C_struct(elts: [ValueRef]) -> ValueRef unsafe {
    ret llvm::LLVMConstStruct(vec::to_ptr(elts), vec::len(elts),
                              False);
}

fn C_named_struct(T: TypeRef, elts: [ValueRef]) -> ValueRef unsafe {
    ret llvm::LLVMConstNamedStruct(T, vec::to_ptr(elts),
                                   vec::len(elts));
}

fn C_array(ty: TypeRef, elts: [ValueRef]) -> ValueRef unsafe {
    ret llvm::LLVMConstArray(ty, vec::to_ptr(elts),
                             vec::len(elts));
}

fn C_bytes(bytes: [u8]) -> ValueRef unsafe {
    ret llvm::LLVMConstString(
        unsafe::reinterpret_cast(vec::to_ptr(bytes)),
        vec::len(bytes), False);
}

fn C_shape(ccx: @crate_ctxt, bytes: [u8]) -> ValueRef {
    let llshape = C_bytes(bytes);
    let llglobal = str::as_buf(ccx.names("shape"), {|buf|
        llvm::LLVMAddGlobal(ccx.llmod, val_ty(llshape), buf)
    });
    llvm::LLVMSetInitializer(llglobal, llshape);
    llvm::LLVMSetGlobalConstant(llglobal, True);
    llvm::LLVMSetLinkage(llglobal,
                         lib::llvm::LLVMInternalLinkage as llvm::Linkage);
    ret llvm::LLVMConstPointerCast(llglobal, T_ptr(T_i8()));
}


pure fn valid_variant_index(ix: uint, cx: @block_ctxt, tag_id: ast::def_id,
                            variant_id: ast::def_id) -> bool {

    // Handwaving: it's ok to pretend this code is referentially
    // transparent, because the relevant parts of the type context don't
    // change. (We're not adding new variants during trans.)
    unchecked{
        let variant =
            ty::tag_variant_with_id(bcx_tcx(cx), tag_id, variant_id);
        ix < vec::len(variant.args)
    }
}

pure fn type_has_static_size(cx: @crate_ctxt, t: ty::t) -> bool {
    !ty::type_has_dynamic_size(cx.tcx, t)
}

pure fn non_ty_var(cx: @crate_ctxt, t: ty::t) -> bool {
    let st = ty::struct(cx.tcx, t);
    alt st {
      ty::ty_var(_) { false }
      _          { true }
    }
}

pure fn returns_non_ty_var(cx: @crate_ctxt, t: ty::t) -> bool {
    non_ty_var(cx, ty::ty_fn_ret(cx.tcx, t))
}

pure fn type_is_tup_like(cx: @block_ctxt, t: ty::t) -> bool {
    let tcx = bcx_tcx(cx);
    ty::type_is_tup_like(tcx, t)
}

// Used to identify cached dictionaries
tag dict_param {
    dict_param_dict(dict_id);
    dict_param_ty(ty::t);
}
type dict_id = @{def: ast::def_id, params: [dict_param]};
fn hash_dict_id(&&dp: dict_id) -> uint {
    let h = syntax::ast_util::hash_def_id(dp.def);
    for param in dp.params {
        h = h << 2u;
        alt param {
          dict_param_dict(d) { h += hash_dict_id(d); }
          dict_param_ty(t) { h += t; }
        }
    }
    h
}

//
// Local Variables:
// mode: rust
// fill-column: 78;
// indent-tabs-mode: nil
// c-basic-offset: 4
// buffer-file-coding-system: utf-8-unix
// End:
//
