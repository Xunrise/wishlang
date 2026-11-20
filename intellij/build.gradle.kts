import org.jetbrains.intellij.platform.gradle.TestFrameworkType
import org.jetbrains.intellij.platform.gradle.IntelliJPlatformType

plugins {
    kotlin("jvm") version "2.3.0"
    id("org.jetbrains.intellij.platform") version "2.19.0"
}

// deploy.sh passes -PbuildStamp so every local build is a newer version the IDE offers as an update.
version = "0.1.0" + (providers.gradleProperty("buildStamp").orNull?.let { ".$it" } ?: "")

repositories {
    mavenCentral()
    // Direct host: behind a corporate proxy the cache-redirector's AWS mirror can answer 403.
    maven("https://packages.jetbrains.team/maven/p/ij/intellij-dependencies")
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
        name = "wishlang"
        changeNotes = """
            <p>First Marketplace release.</p>
            <ul>
              <li>Generated code is tinted, with who wrote it and why on hover.</li>
              <li>Generated code is traced to the problems of the branch's ticket.</li>
              <li>Wish tool window: journal with diffs and reasoning, ticket problem tree.</li>
              <li>Offers to download <code>wishlang</code> when it is not installed.</li>
            </ul>
        """.trimIndent()
        ideaVersion {
            sinceBuild = sinceBuildNumber
            untilBuild = provider { null }
        }
    }
    // Secrets come from the environment (CI secrets or the owner's shell); never commit them.
    signing {
        certificateChain = providers.environmentVariable("CERTIFICATE_CHAIN")
        privateKey = providers.environmentVariable("PRIVATE_KEY")
        password = providers.environmentVariable("PRIVATE_KEY_PASSWORD")
    }
    publishing {
        token = providers.environmentVariable("PUBLISH_TOKEN")
    }
    // ./gradlew verifyPlugin checks against the latest stable IntelliJ IDEA releases since sinceBuild.
    pluginVerification {
        ides {
            select {
                types = listOf(IntelliJPlatformType.IntellijIdea)
                sinceBuild = sinceBuildNumber
            }
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

// Publishes the built zip into -PdeployDir, a folder IntelliJ reads as a custom plugin repository
// (-PdeployUrl is that folder's file:/// URL), and upserts this plugin's entry in its updatePlugins.xml.
// Other plugins' entries in the same file (e.g. Hexagonal Inspector) are kept. Run through ./deploy.sh.
val deploy by tasks.registering {
    group = "intellij platform"
    description = "Publish the plugin to the local custom plugin repository"
    val zip = tasks.buildPlugin.flatMap { it.archiveFile }
    val dir = providers.gradleProperty("deployDir")
    val url = providers.gradleProperty("deployUrl")
    val pluginVersion = version.toString()
    val since = sinceBuildNumber
    inputs.file(zip)
    doLast {
        val target = File(dir.get()).apply { mkdirs() }
        val zipName = "wish.zip"
        zip.get().asFile.copyTo(File(target, zipName), overwrite = true)
        val id = "io.wishlang"
        val entry = """  <plugin id="$id" url="${url.get().trimEnd('/')}/$zipName" version="$pluginVersion">
            |    <name>wishlang</name>
            |    <idea-version since-build="$since"/>
            |  </plugin>""".trimMargin()
        val xml = File(target, "updatePlugins.xml")
        val others = xml.takeIf { it.exists() }?.readText()
            ?.let { Regex("""(?s)<plugin\s.*?</plugin>""").findAll(it).map { m -> m.value } }
            ?.filterNot { it.contains("id=\"$id\"") }?.map { "  $it" }?.toList().orEmpty()
        xml.writeText(
            (listOf("""<?xml version="1.0" encoding="UTF-8"?>""", "<plugins>") + others + entry + "</plugins>")
                .joinToString("\n", postfix = "\n"),
        )
        println("Deployed Wish $pluginVersion to ${target.path}")
    }
}
