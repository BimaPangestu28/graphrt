import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// The sample reuses the repository's files instead of keeping copies:
// the generated UniFFI Kotlin binding and the offline hello workflow.
val repoRoot = rootProject.layout.projectDirectory.dir("../../..")
val workflowAssetsDir = layout.buildDirectory.dir("generated/workflowAssets")

val copyWorkflowAssets by tasks.registering(Sync::class) {
    description = "Bundles examples/hello.yaml as an app asset."
    from(repoRoot.file("examples/hello.yaml"))
    into(workflowAssetsDir)
}

android {
    namespace = "dev.graphrt.sample"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.graphrt.sample"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        // Must match the ABIs built by scripts/build-rust.sh.
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    sourceSets["main"].apply {
        java.srcDir(repoRoot.dir("bindings/kotlin"))
        assets.srcDir(workflowAssetsDir)
        // jniLibs/ (the default location) is filled by scripts/build-rust.sh.
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Unsigned-for-store sample: sign release builds with the debug key so they install.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        // libgraphrt_ffi.so is already stripped by the min-size profile.
        jniLibs.keepDebugSymbols += "**/libgraphrt_ffi.so"
    }
}

kotlin {
    compilerOptions { jvmTarget.set(JvmTarget.JVM_17) }
}

tasks.named("preBuild") { dependsOn(copyWorkflowAssets) }

// Fail early with a clear message instead of a runtime UnsatisfiedLinkError.
val checkNativeLibraries by tasks.registering {
    val jniLibsDir = layout.projectDirectory.dir("src/main/jniLibs")
    doLast {
        val missing = listOf("arm64-v8a", "x86_64").filterNot { abi ->
            jniLibsDir.file("$abi/libgraphrt_ffi.so").asFile.exists()
        }
        if (missing.isNotEmpty()) {
            throw GradleException(
                "libgraphrt_ffi.so missing for $missing. Run examples/mobile/android/scripts/build-rust.sh first.",
            )
        }
    }
}
tasks.named("preBuild") { dependsOn(checkNativeLibraries) }

dependencies {
    // UniFFI's Kotlin bindings call into the native library through JNA.
    implementation("net.java.dev.jna:jna:5.17.0@aar")
}
