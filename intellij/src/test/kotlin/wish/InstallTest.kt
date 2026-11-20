package wish

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class InstallTest {
    @Test
    fun `the target triple follows os and cpu, and is null where releases ship no binary`() {
        assertEquals("aarch64-apple-darwin", targetTriple("Mac OS X", "aarch64"))
        assertEquals("x86_64-apple-darwin", targetTriple("Mac OS X", "x86_64"))
        assertEquals("x86_64-unknown-linux-musl", targetTriple("Linux", "amd64"))
        assertEquals("aarch64-unknown-linux-musl", targetTriple("Linux", "aarch64"))
        assertNull(targetTriple("Windows 11", "amd64"))
        assertNull(targetTriple("Linux", "riscv64"))
    }

    @Test
    fun `the asset is the tarball named for this triple`() {
        val release = """{"assets":[
            {"name":"wishlang-0.2.0-x86_64-apple-darwin.tar.gz","browser_download_url":"https://x/wishlang-0.2.0-x86_64-apple-darwin.tar.gz"},
            {"name":"wishlang-0.2.0-aarch64-apple-darwin.tar.gz.sha256","browser_download_url":"https://x/wishlang-0.2.0-aarch64-apple-darwin.tar.gz.sha256"},
            {"name":"wishlang-0.2.0-aarch64-apple-darwin.tar.gz","browser_download_url":"https://x/wishlang-0.2.0-aarch64-apple-darwin.tar.gz"}
        ]}"""
        assertEquals("https://x/wishlang-0.2.0-aarch64-apple-darwin.tar.gz", assetUrl(release, "aarch64-apple-darwin"))
        assertNull(assetUrl(release, "x86_64-unknown-linux-musl"))
    }
}
