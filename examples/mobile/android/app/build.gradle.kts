import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// The sample uses the SDK module (sdk/android/graphrt: binding, helpers and the
// native libraries) and the repository's workflows instead of keeping copies.
val repoRoot = rootProject.layout.projectDirectory.dir("../../..")
val workflowAssetsDir = layout.buildDirectory.dir("generated/workflowAssets")

val copyWorkflowAssets by tasks.registering(Sync::class) {
    description = "Bundles the offline workflows and the agent's runtime config as app assets."
    from(repoRoot.file("examples/hello.yaml"))
    from(repoRoot.file("examples/mobile/agent.yaml"))
    from(repoRoot.file("examples/mobile/runtime-config.json"))
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
        assets.srcDir(workflowAssetsDir)
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

dependencies {
    // The Graphrt SDK (sdk/android): an app outside this repository uses the AAR
    // of a release instead (see docs/mobile.md).
    implementation(project(":graphrt"))
}
