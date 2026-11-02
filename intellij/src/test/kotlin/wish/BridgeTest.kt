package wish

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Test

class BridgeTest {
    private val wsl = Bridge.of("//wsl.localhost/Ubuntu/home/me/my project")

    @Test
    fun `a project under wsl localhost or wsl$ runs wish in that distro`() {
        assertEquals(Bridge.Wsl("//wsl.localhost/Ubuntu", "Ubuntu"), wsl)
        assertEquals(Bridge.Wsl("//wsl$/Debian", "Debian"), Bridge.of("""\\wsl$\Debian\srv\app"""))
        assertEquals(Bridge.Local, Bridge.of("/home/me/app"))
        assertEquals(Bridge.Local, Bridge.of("C:/Users/me/app"))
        assertEquals(Bridge.Local, Bridge.of(null))
    }

    @Test
    fun `the command enters the project's linux directory through wsl exe`() {
        val cmd = wsl.command("//wsl.localhost/Ubuntu/home/me/my project")
        assertEquals(listOf("wsl.exe", "-d", "Ubuntu", "--cd", "/home/me/my project", "--exec", "bash", "-c"), cmd.dropLast(1))
        assertNull(wsl.directory("//wsl.localhost/Ubuntu/home/me"))
        assertFalse("Windows does not escape quotes inside arguments", cmd.last().contains('"'))
    }

    @Test
    fun `paths map to linux uris and back`() {
        val path = "//wsl.localhost/Ubuntu/home/me/my project/src/Ä.java"
        val uri = wsl.uri(path)
        assertEquals("file:///home/me/my%20project/src/%C3%84.java", uri)
        assertEquals(path, wsl.path(uri))
        assertEquals(path, wsl.path("file:///home/me/my%20project/src/%C3%84.java"))
        assertEquals(uri, serverUri(path))
        assertEquals("file:///home/me/x.java", serverUri("/home/me/x.java"))
    }

    @Test
    fun `parseWsl splits the distro root from the linux path`() {
        assertEquals("//wsl.localhost/Ubuntu" to "/", parseWsl("//wsl.localhost/Ubuntu"))
        assertEquals("//WSL.localhost/Ubuntu" to "/a/b", parseWsl("""\\WSL.localhost\Ubuntu\a\b"""))
        assertNull(parseWsl("//server/share/x"))
    }
}
