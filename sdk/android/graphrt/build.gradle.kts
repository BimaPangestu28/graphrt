import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
    `maven-publish`
    signing
}

// Relative to this module, so the sample app can include it from another build.
val repoRoot = layout.projectDirectory.dir("../../..")
// The repository's version, so the AAR and the crates never disagree.
val graphrtVersion: String = providers.gradleProperty("graphrtVersion").orElse(
    providers.fileContents(repoRoot.file("Cargo.toml")).asText.map { text ->
        Regex("""(?m)^version\s*=\s*"([^"]+)"""").find(text)?.groupValues?.get(1) ?: "0.0.0"
    },
).get()

// The Maven group: a namespace verified on the Central Portal (-PgraphrtGroup=...).
val graphrtGroup: String = providers.gradleProperty("graphrtGroup").orElse("com.graphrt").get()

group = graphrtGroup
version = graphrtVersion

android {
    namespace = "com.graphrt"
    compileSdk = 36

    defaultConfig {
        minSdk = 26
        consumerProguardFiles("consumer-rules.pro")
    }

    sourceSets["main"].apply {
        // The generated UniFFI binding, kept in bindings/ for every language.
        java.srcDir(repoRoot.dir("bindings/kotlin"))
        // jniLibs/ is filled by scripts/build-native.sh.
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        // Already stripped by the min-size profile.
        jniLibs.keepDebugSymbols += "**/libgraphrt_ffi.so"
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
        }
    }
}

kotlin {
    compilerOptions { jvmTarget.set(JvmTarget.JVM_17) }
}

// The ABIs scripts/build-native.sh builds; an AAR without one of them fails at
// run time with UnsatisfiedLinkError, so fail the build instead. A build for
// some ABIs only (the emulator smoke test) passes -Pgraphrt.abis=arm64-v8a,x86_64.
val abis = providers.gradleProperty("graphrt.abis").orElse("arm64-v8a,armeabi-v7a,x86_64").get().split(",")
val checkNativeLibraries by tasks.registering {
    val jniLibsDir = layout.projectDirectory.dir("src/main/jniLibs")
    doLast {
        val missing = abis.filterNot { jniLibsDir.file("$it/libgraphrt_ffi.so").asFile.exists() }
        if (missing.isNotEmpty()) {
            throw GradleException("libgraphrt_ffi.so missing for $missing. Run sdk/android/scripts/build-native.sh first.")
        }
    }
}
tasks.named("preBuild") { dependsOn(checkNativeLibraries) }

dependencies {
    // UniFFI's Kotlin bindings call the native library through JNA, and its async
    // functions (ToolProvider.callTool) are coroutines.
    api("net.java.dev.jna:jna:5.17.0@aar")
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
}

publishing {
    publications {
        register<MavenPublication>("release") {
            groupId = graphrtGroup
            artifactId = "graphrt-android"
            version = graphrtVersion
            afterEvaluate { from(components["release"]) }
            pom {
                name.set("Graphrt for Android")
                description.set("Run Graphrt agent workflows inside an Android app.")
                url.set("https://github.com/Graphrt/graphrt")
                licenses {
                    license {
                        name.set("Apache-2.0")
                        url.set("https://www.apache.org/licenses/LICENSE-2.0")
                    }
                }
                developers {
                    developer {
                        id.set("graphrt")
                        name.set("Graphrt")
                    }
                }
                scm {
                    url.set("https://github.com/Graphrt/graphrt")
                    connection.set("scm:git:https://github.com/Graphrt/graphrt.git")
                }
            }
        }
    }
    repositories {
        // A local repository the release workflow zips for the Maven Central
        // upload (gated off until the owner sets up the namespace and signing).
        maven {
            name = "staging"
            url = uri(layout.buildDirectory.dir("staging-repo"))
        }
    }
}

// Signs only when a key is configured (ORG_GRADLE_PROJECT_signingKey and
// ORG_GRADLE_PROJECT_signingPassword in the release workflow).
signing {
    val signingKey = providers.gradleProperty("signingKey").orNull
    val signingPassword = providers.gradleProperty("signingPassword").orNull
    isRequired = signingKey != null
    if (signingKey != null) {
        useInMemoryPgpKeys(signingKey, signingPassword)
        sign(publishing.publications["release"])
    }
}
