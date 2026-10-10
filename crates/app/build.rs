fn main() {
    #[cfg(feature = "servo")]
    {
        use gl_generator::{Api, Fallbacks, Profile, Registry, StructGenerator};
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let mut file = std::fs::File::create(out.join("gl_bindings.rs")).unwrap();
        // Same GL extension set as Slint's servo example: needed for Vulkan/GL texture sharing.
        Registry::new(
            Api::Gles2,
            (3, 0),
            Profile::Core,
            Fallbacks::All,
            ["GL_EXT_memory_object", "GL_EXT_memory_object_fd", "GL_EXT_semaphore", "GL_EXT_semaphore_fd"],
        )
        .write_bindings(StructGenerator, &mut file)
        .unwrap();
    }
    println!("cargo:rerun-if-changed=build.rs");
    // Debug info lets the story tests find UI elements by accessible label.
    let debug = std::env::var("PROFILE").as_deref() == Ok("debug");
    let config = slint_build::CompilerConfiguration::new().with_debug_info(debug);
    slint_build::compile_with_config("ui/app.slint", config).unwrap();
}

