use std::{env, path::Path};

use built::Options;

fn main() {
    let mut options = Options::default();
    options.set_compiler(false).set_ci(false).set_features(false).set_cfg(false);
    built::write_built_file_with_opts(
        &options,
        env::var("CARGO_MANIFEST_DIR").unwrap().as_ref(),
        &Path::new(&env::var("OUT_DIR").unwrap()).join("built.rs"),
    )
    .expect("Failed to acquire build-time information");
}
