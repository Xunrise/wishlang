package wish

import java.io.File
import java.net.URI

/**
 * Where `wish lsp` runs, and how a file is named on each side.
 *
 * A Windows IDE that opens a project under `\\wsl.localhost\<distro>\…` (IntelliJ's path:
 * `//wsl.localhost/<distro>/…`) cannot run a Linux binary itself, so [Wsl] starts `wish lsp` inside
 * that distro through `wsl.exe` and maps between the IDE's UNC paths and the server's Linux paths.
 * Everything else uses [Local].
 */
sealed interface Bridge {
    /** The command line that starts `wish lsp` for the project at [root] (an IDE path). */
    fun command(root: String): List<String>

    /** The working directory for [command], or null when the command sets it itself. */
    fun directory(root: String): File?

    /** The server's URI for an IDE path. */
    fun uri(path: String): String

    /** The IDE path for a server URI. */
    fun path(uri: String): String

    object Local : Bridge {
        override fun command(root: String) = listOf(wishBinary().path, "lsp")
        override fun directory(root: String) = File(root)
        override fun uri(path: String): String = File(path).toPath().toUri().toString()
        override fun path(uri: String): String = File(URI(uri)).path
    }

    /** `wish lsp` inside the WSL distro [distro]; [prefix] is the IDE's path to the distro's `/`, e.g. `//wsl.localhost/Ubuntu`. */
    data class Wsl(val prefix: String, val distro: String) : Bridge {
        override fun command(root: String) = listOf(
            "wsl.exe", "-d", distro, "--cd", linux(root), "--exec", "bash", "-c", LAUNCH,
        )

        override fun directory(root: String): File? = null

        // URI(scheme, host = "", path, fragment) renders as file:///path, percent-encoding what needs it.
        override fun uri(path: String): String = URI("file", "", linux(path), null).toASCIIString()

        override fun path(uri: String): String = prefix + URI(uri).path

        /** The Linux path of an IDE path inside this distro. */
        fun linux(path: String): String = parseWsl(path)?.second ?: path

        companion object {
            /**
             * Runs in WSL's bash. `.bashrc` puts Java, Maven, node and `claude` on PATH but returns early unless the
             * shell is interactive, so wish runs under `bash -lic`. Whatever the rc files print goes to stderr (fd 1 is
             * parked on fd 3 meanwhile), keeping stdout clean for LSP. `$WISH_BIN` is read on the Linux side.
             * No double quotes: Java on Windows wraps an argument in them without escaping embedded ones.
             */
            const val LAUNCH =
                "exec 3>&1 1>&2; exec bash -lic 'exec 1>&3 3>&-; exec \${WISH_BIN:-\$(command -v wishlang || echo \$HOME/.cargo/bin/wishlang)} lsp'"
        }
    }

    companion object {
        /** The bridge for a project at the IDE path [root]. */
        fun of(root: String?): Bridge =
            root?.let(::parseWsl)?.let { (prefix, _) -> Wsl(prefix, prefix.substringAfterLast('/')) } ?: Local
    }
}

private val WSL_UNC = Regex("""^[/\\]{2}(wsl\$|wsl\.localhost)[/\\]([^/\\]+)(.*)$""", RegexOption.IGNORE_CASE)

/** Splits `//wsl.localhost/<distro>/rest` (or `\\wsl$\<distro>\rest`) into `//host/<distro>` and `/rest`. */
fun parseWsl(path: String): Pair<String, String>? {
    val m = WSL_UNC.matchEntire(path) ?: return null
    val (host, distro, rest) = m.destructured
    val linux = rest.replace('\\', '/').ifEmpty { "/" }
    return "//$host/$distro" to linux
}

/** The server's URI for [file]: a Linux `file:///…` URI when the file lives in WSL, else the local one. */
fun serverUri(path: String): String = parseWsl(path)?.let { (prefix, _) -> Bridge.of(prefix).uri(path) } ?: Bridge.Local.uri(path)
