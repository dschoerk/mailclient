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
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        build_dex();
    }
    // Debug info lets the story tests find UI elements by accessible label.
    let debug = std::env::var("PROFILE").as_deref() == Ok("debug");
    let config = slint_build::CompilerConfiguration::new().with_debug_info(debug);
    slint_build::compile_with_config("ui/app.slint", config).unwrap();
}

/// Compiles the Java side of the Android HTML view (java/) to classes.dex, which the app
/// loads at runtime (platform/android_html.rs): the APK itself stays a NativeActivity without code.
fn build_dex() {
    use std::path::PathBuf;
    use std::process::Command;
    println!("cargo:rerun-if-changed=java");
    println!("cargo:rerun-if-env-changed=ANDROID_HOME");
    println!("cargo:rerun-if-env-changed=JAVA_HOME");
    let env = |name: &str| PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for Android builds")));
    let (sdk, jdk) = (env("ANDROID_HOME"), env("JAVA_HOME"));
    let android_jar = sdk.join("platforms/android-35/android.jar");
    let d8 = sdk.join("build-tools/35.0.0/d8");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let classes = out.join("classes");
    let _ = std::fs::remove_dir_all(&classes);
    let run = |command: &mut Command| {
        let status = command.status().unwrap_or_else(|e| panic!("running {command:?}: {e}"));
        assert!(status.success(), "{command:?} failed");
    };
    run(Command::new(jdk.join("bin/javac"))
        .args(["--release", "11", "-Xlint:-options", "-classpath"])
        .arg(&android_jar)
        .arg("-d")
        .arg(&classes)
        .arg("java/dev/mailclient/MailWebView.java"));
    let class_files: Vec<PathBuf> = std::fs::read_dir(classes.join("dev/mailclient"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    // d8 is a script that runs `java` from PATH.
    let path = format!("{}:{}", jdk.join("bin").display(), std::env::var("PATH").unwrap_or_default());
    run(Command::new(d8)
        .env("PATH", path)
        .args(["--min-api", "28", "--release", "--lib"])
        .arg(&android_jar)
        .arg("--output")
        .arg(&out)
        .args(&class_files));
}
