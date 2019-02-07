use rustc_data_structures::indexed_vec::IndexVec;
use rustc::ty::{self, TyCtxt, Ty, ParamTy, TypeFoldable, Instance, ParamEnv};
use rustc::ty::fold::TypeFolder;
use rustc::ty::subst::{Kind, UnpackedKind};
use rustc::ty::layout::{LayoutCx, LayoutOf};
use rustc::mir::{Mir, Rvalue, Location};
use rustc::mir::visit::{Visitor, TyContext};

/// Replace substs which aren't used by the function with TyError,
/// so that it doesn't end up in the binary multiple times
/// For example in the code
///
/// ```rust
/// fn foo<T>() { } // here, T is clearly unused =)
///
/// fn main() {
///     foo::<u32>();
///     foo::<u64>();
/// }
/// ```
///
/// `foo::<u32>` and `foo::<u64>` are collapsed to `foo::<{some dummy}>`,
/// because codegen for `foo` doesn't depend on the Subst for T.
pub(crate) fn collapse_interchangable_instances<'a, 'tcx>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    mut instance: Instance<'tcx>
) -> Instance<'tcx> {
    info!("replace_unused_substs_with_ty_error({:?})", instance);

    if instance.substs.is_noop() || !tcx.is_mir_available(instance.def_id()) {
        return instance;
    }
    match instance.ty(tcx).sty {
        ty::FnDef(def_id, _) => {
            if tcx.lang_items().items().iter().find(|l|**l == Some(def_id)).is_some() {
                return instance; // Lang items dont work otherwise
            }
        }
        _ => return instance, // Closures dont work otherwise
    }

    let used_substs = used_substs_for_instance(tcx, instance);
    instance.substs = tcx.intern_substs(&instance.substs.into_iter().enumerate().map(|(i, subst)| {
        if let UnpackedKind::Type(ty) = subst.unpack() {
            let ty = match used_substs.parameters[ParamIdx(i as u32)] {
                ParamUsage::Unused => {
                    if false /*param.name.as_str().starts_with("<")*/ {
                        ty.into()
                    } else {
                        #[allow(unused_mut)]
                        let mut mir = Vec::new();
                        ::util::write_mir_pretty(tcx, Some(instance.def_id()), &mut mir).unwrap();
                        let mut generics = Some(tcx.generics_of(instance.def_id()));
                        let mut pretty_generics = String::new();
                        loop {
                            if let Some(ref gen) = generics {
                                for ty in &gen.params {
                                    pretty_generics.push_str(&format!(
                                        "{}:{} at {:?}, ",
                                        ty.index,
                                        ty.name,
                                        /*tcx.def_span(ty.def_id)*/ "???"
                                    ));
                                }
                            } else {
                                break;
                            }
                            generics = generics.and_then(|gen|gen.parent)
                                .map(|def_id|tcx.generics_of(def_id));
                        }
                        tcx.sess.warn(&format!(
                            "Unused subst {} for {:?}<{}>\n with mir: {}",
                            i,
                            instance,
                            pretty_generics,
                            String::from_utf8_lossy(&mir)
                        ));
                        tcx.mk_ty(ty::UnusedParam)
                    }
                }
                ParamUsage::LayoutUsed => {
                    let layout_cx = LayoutCx {
                        tcx,
                        param_env: ParamEnv::reveal_all(),
                    };
                    let layout = layout_cx.layout_of(ty).unwrap();
                    // FIXME: wrong wrong wrong
                    tcx.mk_ty(ty::LayoutOnlyParam(layout.size, layout.align.abi))
                }
                ParamUsage::Used => ty.into(),
            };
            Kind::from(ty)
        } else {
            (*subst).clone()
        }
    }).collect::<Vec<_>>());
    info!("replace_unused_substs_with_ty_error(_) -> {:?}", instance);
    instance
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct ParamIdx(u32);

impl ::rustc_data_structures::indexed_vec::Idx for ParamIdx {
    fn new(idx: usize) -> Self {
        assert!(idx < ::std::u32::MAX as usize);
        ParamIdx(idx as u32)
    }

    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
enum ParamUsage {
    Unused = 0,
    #[allow(dead_code)]
    LayoutUsed = 1,
    Used = 2,
}

impl_stable_hash_for! { enum self::ParamUsage { Unused, LayoutUsed, Used } }

#[derive(Debug, Default, Clone)]
pub struct ParamsUsage {
    parameters: IndexVec<ParamIdx, ParamUsage>,
}

impl_stable_hash_for! { struct ParamsUsage { parameters } }

impl ParamsUsage {
    fn new(len: usize) -> ParamsUsage {
        ParamsUsage {
            parameters: IndexVec::from_elem_n(ParamUsage::Unused, len),
        }
    }
}

struct SubstsVisitor<'a, 'gcx: 'a + 'tcx, 'tcx: 'a>(
    TyCtxt<'a, 'gcx, 'tcx>,
    &'tcx Mir<'tcx>,
    ParamsUsage,
);

impl<'a, 'gcx: 'a + 'tcx, 'tcx: 'a> Visitor<'tcx> for SubstsVisitor<'a, 'gcx, 'tcx> {
    fn visit_mir(&mut self, mir: &Mir<'tcx>) {
        for promoted in &mir.promoted {
            self.visit_mir(promoted);
        }
        self.super_mir(mir);
    }

    fn visit_ty(&mut self, ty: &Ty<'tcx>, _: TyContext) {
        self.fold_ty(ty);
    }

    /*
    fn visit_const(&mut self, constant: &&'tcx ty::Const<'tcx>, _location: Location) {
        if let ConstVal::Unevaluated(_def_id, substs) = constant.val {
            for subst in substs {
                if let UnpackedKind::Type(ty) = subst.unpack() {
                    ty.fold_with(self);
                }
            }
        }
    }
    */

    fn visit_rvalue(&mut self, rvalue: &Rvalue<'tcx>, location: Location) {
        let tcx = self.0;
        match *rvalue {
            Rvalue::Cast(_kind, ref op, ty) => {
                self.fold_ty(op.ty(&self.1.local_decls, tcx));
                self.fold_ty(ty);
            }
            _ => {}
        }
        self.super_rvalue(rvalue, location);
    }
}

impl<'a, 'gcx: 'a + 'tcx, 'tcx: 'a> TypeFolder<'gcx, 'tcx> for SubstsVisitor<'a, 'gcx, 'tcx> {
    fn tcx<'b>(&'b self) -> TyCtxt<'b, 'gcx, 'tcx> {
        self.0
    }
    fn fold_ty(&mut self, ty: Ty<'tcx>) -> Ty<'tcx> {
        if !ty.needs_subst() {
            return ty;
        }
        match ty.sty {
            ty::Param(param) => {
                self.2.parameters[ParamIdx(param.idx)] = ParamUsage::Used;
            }
            _ => {}
        }
        ty.super_fold_with(self)
    }
}

fn used_substs_for_instance<'a, 'tcx: 'a>(
    tcx: TyCtxt<'a ,'tcx, 'tcx>,
    instance: Instance<'tcx>,
) -> ParamsUsage {
    let mir = tcx.instance_mir(instance.def);
    let generics = tcx.generics_of(instance.def_id());
    let sig = instance.fn_sig(tcx);
    let sig = tcx.normalize_erasing_late_bound_regions(ty::ParamEnv::reveal_all(), &sig);
    let mut substs_visitor = SubstsVisitor(tcx, mir, ParamsUsage::new(instance.substs.len()));
    //substs_visitor.visit_mir(mir);
    mir.fold_with(&mut substs_visitor);
    for ty in sig.inputs().iter() {
        ty.fold_with(&mut substs_visitor);
    }
    for param_def in &generics.params {
        if ParamTy::for_def(param_def).is_self() {
            // The self parameter is important for trait selection
            (substs_visitor.2).parameters[ParamIdx(param_def.index)] = ParamUsage::Used;
        }
    }
    sig.output().fold_with(&mut substs_visitor);
    substs_visitor.2
}
