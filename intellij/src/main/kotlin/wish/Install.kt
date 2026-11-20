package wish

import com.intellij.ide.BrowserUtil
import com.intellij.notification.NotificationAction
import com.intellij.notification.NotificationGroupManager
import com.intellij.notification.NotificationType
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.service
import com.intellij.openapi.progress.ProgressIndicator
import com.intellij.openapi.progress.ProgressManager
import com.intellij.openapi.progress.Task
import com.intellij.openapi.project.Project
import com.intellij.util.io.HttpRequests
import kotlinx.coroutines.runBlocking
import java.io.File
import java.nio.file.Files

const val INSTALL_DOCS = "https://xunrise.github.io/wishlang/#install"
private const val LATEST_RELEASE = "https://api.github.com/repos/Xunrise/wishlang/releases/latest"

/** The release asset's target triple for this machine, or null where releases ship no binary. */
fun targetTriple(os: String = System.getProperty("os.name"), arch: String = System.getProperty("os.arch")): String? {
    val cpu = when (arch.lowercase()) {
        "aarch64", "arm64" -> "aarch64"
        "amd64", "x86_64" -> "x86_64"
        else -> return null
    }
    return when {
        os.startsWith("Mac") -> "$cpu-apple-darwin"
        os.startsWith("Linux") -> "$cpu-unknown-linux-musl"
        else -> null
    }
}

/** The download URL of this machine's `.tar.gz` in a GitHub release JSON, matched by the `-<triple>.tar.gz` suffix. */
fun assetUrl(releaseJson: String, triple: String): String? =
    Regex(""""browser_download_url"\s*:\s*"([^"]*-${Regex.escape(triple)}\.tar\.gz)"""").find(releaseJson)?.groupValues?.get(1)

/**
 * Shown when no `wishlang` binary is found: links to the install docs and, where a release binary exists
 * for this machine, offers to download it into ~/.local/bin (the first place [wishBinary] looks).
 */
fun notifyMissing(project: Project) {
    val n = NotificationGroupManager.getInstance().getNotificationGroup("Wish").createNotification(
        "wishlang is not installed",
        "The Wish plugin needs the <code>wishlang</code> command-line tool, and Claude Code (<code>claude</code>) on PATH.",
        NotificationType.WARNING,
    )
    n.addAction(NotificationAction.createSimple("How to install") { BrowserUtil.browse(INSTALL_DOCS) })
    targetTriple()?.let { triple ->
        n.addAction(NotificationAction.createSimpleExpiring("Download to ~/.local/bin") { download(project, triple) })
    }
    n.notify(project)
}

private fun download(project: Project, triple: String) {
    object : Task.Backgroundable(project, "Downloading wishlang", true) {
        override fun run(indicator: ProgressIndicator) {
            val release = HttpRequests.request(LATEST_RELEASE).accept("application/vnd.github+json").readString(indicator)
            val url = assetUrl(release, triple) ?: error("The latest wishlang release has no build for $triple.")
            val tmp = Files.createTempDirectory("wishlang")
            try {
                val archive = tmp.resolve("wishlang.tar.gz").toFile()
                HttpRequests.request(url).saveToFile(archive, indicator)
                val tar = ProcessBuilder("tar", "-xzf", archive.path, "-C", tmp.toString()).redirectErrorStream(true).start()
                check(tar.waitFor() == 0) { "tar failed: ${tar.inputStream.bufferedReader().readText()}" }
                val bin = tmp.toFile().walk().firstOrNull { it.isFile && it.name == "wishlang" } ?: error("No wishlang binary in $url.")
                val target = File(System.getProperty("user.home"), ".local/bin/wishlang")
                target.parentFile.mkdirs()
                bin.copyTo(target, overwrite = true)
                target.setExecutable(true)
            } finally {
                tmp.toFile().deleteRecursively()
            }
        }

        override fun onSuccess() {
            project.service<WishService>().notify(org.eclipse.lsp4j.MessageType.Info, "Installed wishlang to ~/.local/bin.")
            ApplicationManager.getApplication().executeOnPooledThread { runBlocking { project.service<WishService>().start() } }
        }

        override fun onThrowable(error: Throwable) {
            NotificationGroupManager.getInstance().getNotificationGroup("Wish")
                .createNotification("Could not download wishlang", error.message.orEmpty(), NotificationType.ERROR)
                .addAction(NotificationAction.createSimple("How to install") { BrowserUtil.browse(INSTALL_DOCS) })
                .notify(project)
        }
    }.queue()
}
