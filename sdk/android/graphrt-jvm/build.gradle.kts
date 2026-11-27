import org.jetbrains.kotlin.gradle.dsl.JvmTarget

// The sources of :graphrt (binding and helpers) compiled for the JVM, so their
// tests and the docs/mobile.md snippets run on any machine against a host build
// of libgraphrt_ffi (scripts/test-jvm.sh builds it with the `sdk` features).
plugins {
    id("org.jetbrains.kotlin.jvm")
}

val repoRoot = rootProject.layout.projectDirectory.dir("../..")

kotlin {
    jvmToolchain(17)
    compilerOptions { jvmTarget.set(JvmTarget.JVM_17) }
    sourceSets["main"].kotlin.srcDir(repoRoot.dir("bindings/kotlin"))
    sourceSets["main"].kotlin.srcDir(rootProject.layout.projectDirectory.dir("graphrt/src/main/kotlin"))
}

dependencies {
    implementation("net.java.dev.jna:jna:5.17.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
    testImplementation(kotlin("test"))
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.10.2")
}

tasks.test {
    useJUnitPlatform()
    val libDir = providers.environmentVariable("GRAPHRT_LIB_DIR")
        .orElse(repoRoot.dir("target/debug").asFile.absolutePath)
    systemProperty("jna.library.path", libDir.get())
    // Shared files of the repository the tests run (examples/mobile/agent.yaml...).
    systemProperty("graphrt.repo", repoRoot.asFile.absolutePath)
    testLogging {
        events("passed", "failed", "skipped")
        exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
    }
}
