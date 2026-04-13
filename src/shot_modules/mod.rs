use crate::models::ShotMeta;
use image::DynamicImage;
use std::fs;
use std::path::Path;

mod enhanced;
mod framed;
mod original;
mod polaroid;

#[derive(Debug, Clone)]
pub(crate) struct ShotOutputVariant {
    pub module_id: &'static str,
    pub module_name: &'static str,
    pub rel_url: String,
    pub abs_path: String,
}

#[derive(Debug, Clone, Copy)]
struct ShotOutputModule {
    module_id: &'static str,
    module_name: &'static str,
    render: fn(&DynamicImage, &ShotMeta) -> DynamicImage,
}

const SHOT_OUTPUT_MODULES: &[ShotOutputModule] = &[
    ShotOutputModule {
        module_id: "original",
        module_name: "Original",
        render: original::render,
    },
    ShotOutputModule {
        module_id: "framed",
        module_name: "Framed",
        render: framed::render,
    },
    ShotOutputModule {
        module_id: "enhanced",
        module_name: "Enhanced",
        render: enhanced::render,
    },
    ShotOutputModule {
        module_id: "polaroid",
        module_name: "Polaroid",
        render: polaroid::render,
    },
];

pub(crate) fn build_shot_output_variants(
    session_dir: &Path,
    shot: &ShotMeta,
) -> Vec<ShotOutputVariant> {
    let source_abs = session_dir.join(&shot.dest_rel_path);
    let source_img = match image::open(&source_abs) {
        Ok(img) => img,
        Err(_) => return Vec::new(),
    };

    let out_root = session_dir.join("screenshots").join("derived");
    if fs::create_dir_all(&out_root).is_err() {
        return Vec::new();
    }

    let mut variants = Vec::<ShotOutputVariant>::new();
    for module in SHOT_OUTPUT_MODULES {
        let out_name = format!(
            "shot-{id:03}__{module}.png",
            id = shot.shot_id,
            module = module.module_id
        );
        let out_abs = out_root.join(&out_name);
        let out_rel = format!("screenshots/derived/{out_name}");

        let rendered = (module.render)(&source_img, shot);
        if rendered.save(&out_abs).is_err() {
            continue;
        }

        variants.push(ShotOutputVariant {
            module_id: module.module_id,
            module_name: module.module_name,
            rel_url: out_rel,
            abs_path: out_abs.display().to_string(),
        });
    }

    variants
}
