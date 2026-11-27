// The Android SDK of Graphrt: `:graphrt` builds the AAR (Kotlin bindings, helpers
// and libgraphrt_ffi.so for every ABI); `:graphrt-jvm` compiles the same sources
// for the JVM and runs their tests and the docs/mobile.md snippets against a
// host build of the library (no emulator needed).
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "graphrt-android"
include(":graphrt", ":graphrt-jvm")
