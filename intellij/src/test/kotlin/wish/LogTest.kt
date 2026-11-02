package wish

import com.google.gson.Gson
import com.intellij.testFramework.fixtures.BasePlatformTestCase
import java.io.File
import javax.swing.tree.DefaultMutableTreeNode

private fun DefaultMutableTreeNode.kids() = children().toList().map { it as DefaultMutableTreeNode }

class LogTest : BasePlatformTestCase() {
    private val log = Gson().fromJson(File("src/test/testData/log.json").readText(), LogResult::class.java)

    fun testFileAppearsUnderEveryProblemItServes() {
        val groups = logGroups(log.entries[1], log.problems).kids()
        assertEquals(listOf("P1", UNTRACED), groups.map { (it.userObject as ProblemNode).id })
        assertEquals("The demo window does not open", (groups[0].userObject as ProblemNode).text)
        for (group in groups) assertEquals(listOf("src/main/java/demo/App.java"), group.kids().map { (it.userObject as FileNode).path })
        assertEquals(listOf("App.create"), groups[0].kids().single().kids().map { (it.userObject as TItem).item })
        assertEquals(listOf("App.run"), groups[1].kids().single().kids().map { (it.userObject as TItem).item })
    }

    fun testOneItemServingTwoProblemsIsListedUnderBoth() {
        val why = Why("App.create", "x", listOf("P1", "P2"), "src/main/java/demo/App.java", 11)
        val groups = logGroups(Entry(why = listOf(why)), log.problems).kids()
        assertEquals(listOf("P1", "P2"), groups.map { (it.userObject as ProblemNode).id })
        groups.forEach { assertEquals(1, it.kids().single().kids().size) }
    }

    fun testWishFilesAreHidden() {
        val why = listOf(Why("Journal", "x", listOf("P1"), ".wish/journal.md", 1), Why("App.create", "y", listOf("P1"), "src/App.java", 2))
        val files = logGroups(Entry(why = why), log.problems).kids().single().kids().map { (it.userObject as FileNode).path }
        assertEquals(listOf("src/App.java"), files)
        assertEquals(listOf("pom.xml"), visibleFiles(listOf(FileStat(".wish/journal.md"), FileStat("pom.xml"))).map { it.path })
    }

    fun testEntriesWithoutWhyHaveNoGroups() {
        assertTrue(logGroups(log.entries[3], log.problems).kids().isEmpty())
    }
}
