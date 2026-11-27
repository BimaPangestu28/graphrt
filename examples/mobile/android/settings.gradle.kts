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

rootProject.name = "graphrt-android-sample"
include(":app", ":graphrt")
// The SDK module, built from this repository.
project(":graphrt").projectDir = file("../../../sdk/android/graphrt")
