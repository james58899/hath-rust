use vergen_gix::{Emitter, GixBuilder};

fn main() {
    if let Ok(gix) = GixBuilder::default().sha(true).build() {
        if Emitter::new().fail_on_error().add_instructions(&gix).and_then(|e| e.emit()).is_ok() {
            return;
        }
    }

    // Fallback
    println!("cargo:rustc-env=VERGEN_GIT_SHA=unknown");
}
