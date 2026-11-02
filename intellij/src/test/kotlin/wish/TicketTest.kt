package wish

import com.google.gson.Gson
import com.intellij.testFramework.fixtures.BasePlatformTestCase
import java.io.File
import javax.swing.tree.DefaultMutableTreeNode

class TicketTest : BasePlatformTestCase() {
    private val ticket = Gson().fromJson(File("src/test/testData/ticket.json").readText(), TicketResult::class.java)

    private fun DefaultMutableTreeNode.kids() = children().toList().map { it as DefaultMutableTreeNode }

    private fun label(node: DefaultMutableTreeNode) = when (val o = node.userObject) {
        is TicketNode -> "ticket ${o.key} ${o.title} (${o.cycles})"
        is ProblemNode -> "p:${o.id}"
        is FileNode -> "f:${o.path}"
        is TItem -> "i:${o.item}"
        else -> "?"
    }

    private fun describe(node: DefaultMutableTreeNode, depth: Int = 0): List<String> =
        listOf("  ".repeat(depth) + label(node)) + node.kids().flatMap { describe(it, depth + 1) }

    fun testTreeIsTicketProblemsUntracedFilesItems() {
        val app = "f:src/main/java/demo/App.java"
        assertEquals(
            listOf(
                "ticket DEMO-1 Open the demo window (2)",
                "  p:P1", "    $app", "      i:App.create",
                "  p:P2", "    $app", "      i:App.create",
                "  p:Untraced", "    $app", "      i:App.run",
            ),
            describe(ticketTree(ticket)),
        )
    }

    fun testProblemWithoutCodeIsStillListed() {
        val bare = TicketResult(key = "K-1", problems = listOf(Problem("P1", "nothing yet")))
        val problem = ticketTree(bare).kids().single()
        assertEquals("ticket/p:P1", selectionKey(problem))
        assertTrue(problem.kids().isEmpty())
    }

    fun testSelectionKeySurvivesReload() {
        val before = findNode(ticketTree(ticket)) { (it.userObject as? TItem)?.item == "App.run" }!!
        val key = selectionKey(before)
        assertEquals("ticket/p:Untraced/f:src/main/java/demo/App.java/i:App.run", key)

        val after = findNode(ticketTree(ticket)) { selectionKey(it) == key }
        assertNotNull(after)
        assertNotSame(before, after)
    }

    fun testSameItemUnderTwoProblemsHasDistinctKeys() {
        val keys = ticketTree(ticket).preorderEnumeration().toList().map { selectionKey(it as DefaultMutableTreeNode) }
        assertEquals(keys.size, keys.toSet().size)
    }
}
