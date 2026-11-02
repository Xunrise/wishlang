import org.jetbrains.intellij.platform.gradle.TestFrameworkType

plugins {
    kotlin("jvm") version "2.3.0"
    id("org.jetbrains.intellij.platform") version "2.19.0"
}

repositories {
    mavenCentral()
    intellijPlatform { defaultRepositories() }
}

val platformVersion = providers.gradleProperty("platformVersion").get()
val sinceBuildNumber = providers.gradleProperty("sinceBuild").get()
val lsp4jVersion = providers.gradleProperty("lsp4jVersion").get()

// -PlocalIde="/Applications/IntelliJ IDEA.app" builds against an installed IDE instead of downloading the platform.
val localIde = providers.gradleProperty("localIde")

dependencies {
    intellijPlatform {
        if (localIde.isPresent) local(localIde.get()) else intellijIdea(platformVersion)
        bundledPlugin("com.intellij.java")
        bundledModule("intellij.platform.vcs.impl") // SimpleAsyncChangesBrowser
        testFramework(TestFrameworkType.Platform)
    }
    // The platform ships gson; lsp4j is the only thing bundled.
    implementation("org.eclipse.lsp4j:org.eclipse.lsp4j:$lsp4jVersion") { exclude(group = "com.google.code.gson") }
    testImplementation("junit:junit:4.13.2")
}

kotlin { jvmToolchain(25) }

intellijPlatform {
    buildSearchableOptions = false
    pluginConfiguration {
        name = "Wish"
        ideaVersion {
            sinceBuild = sinceBuildNumber
            untilBuild = provider { null }
        }
    }
}

tasks.runIde {
    // ./gradlew runIde -PdemoProject=/path/to/project [-PdemoFile=/path/to/File.java]
    args = listOfNotNull(providers.gradleProperty("demoProject").orNull, providers.gradleProperty("demoFile").orNull)
}

// The Ultimate stub plugin registers a startup activity that the platform test framework cannot instantiate.
tasks.prepareTestSandbox {
    doLast {
        sandboxConfigDirectory.get().asFile.resolve("disabled_plugins.txt").writeText("com.intellij.modules.ultimate\n")
    }
}
