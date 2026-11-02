package wish

import com.intellij.icons.AllIcons
import com.intellij.openapi.Disposable
import com.intellij.openapi.actionSystem.ActionManager
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.actionSystem.DefaultActionGroup
import com.intellij.openapi.actionSystem.ToggleAction
import com.intellij.openapi.components.service
import com.intellij.openapi.fileTypes.FileTypeManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.vcs.changes.Change
import com.intellij.openapi.vcs.changes.ui.SimpleAsyncChangesBrowser
import com.intellij.ui.ColoredTableCellRenderer
import com.intellij.ui.ColoredTreeCellRenderer
import com.intellij.ui.OnePixelSplitter
import com.intellij.ui.SimpleTextAttributes
import com.intellij.ui.components.JBLabel
import com.intellij.ui.components.JBScrollPane
import com.intellij.ui.table.JBTable
import com.intellij.ui.treeStructure.Tree
import com.intellij.util.ui.JBUI
import com.intellij.util.ui.UIUtil
import java.awt.BorderLayout
import java.awt.CardLayout
import java.awt.FlowLayout
import javax.swing.Box
import javax.swing.BoxLayout
import javax.swing.JButton
import javax.swing.JComponent
import javax.swing.JPanel
import javax.swing.JTable
import javax.swing.JTree
import javax.swing.ListSelectionModel
import javax.swing.table.AbstractTableModel
import javax.swing.tree.DefaultMutableTreeNode
import javax.swing.tree.DefaultTreeModel

// ---- the problem → file → item model, shared with the Ticket tab --------------------------------

/** Id of the group for generated items that serve no problem of the ticket. */
const val UNTRACED = "Untraced"

class TicketNode(val key: String, val title: String, val cycles: Int)
class ProblemNode(val id: String, val text: String, val count: Int)
class FileNode(val path: String, val count: Int)

private const val REMOVED_FILE = "(removed)"

/**
 * One log entry's reasons grouped problem → file → item. A file that serves several problems sits under each of them,
 * reasons that serve none go under [UNTRACED], and `.wish/` files are left out.
 */
fun logGroups(entry: Entry, problems: List<ProblemText>): DefaultMutableTreeNode {
    val texts = problems.associate { it.id to it.text }
    val groups = LinkedHashMap<String, MutableList<Why>>()
    texts.keys.forEach { groups[it] = mutableListOf() }
    for (why in entry.why.filterNot { isWishFile(it.file.orEmpty()) }) {
        why.problems.ifEmpty { listOf(UNTRACED) }.forEach { groups.getOrPut(it) { mutableListOf() }.add(why) }
    }
    val root = DefaultMutableTreeNode()
    for ((id, rows) in groups.filterValues { it.isNotEmpty() }) {
        val problem = DefaultMutableTreeNode(ProblemNode(id, texts[id].orEmpty(), rows.size))
        for ((file, inFile) in rows.groupBy { it.file ?: REMOVED_FILE }) {
            val node = DefaultMutableTreeNode(FileNode(file, inFile.size))
            inFile.forEach { node.add(DefaultMutableTreeNode(TItem(it.item, it.file.orEmpty(), it.line ?: 1))) }
            problem.add(node)
        }
        root.add(problem)
    }
    return root
}

internal class NodeRenderer(private val counts: Boolean = false) : ColoredTreeCellRenderer() {
    override fun customizeCellRenderer(tree: JTree, value: Any?, selected: Boolean, expanded: Boolean, leaf: Boolean, row: Int, hasFocus: Boolean) {
        when (val node = (value as? DefaultMutableTreeNode)?.userObject) {
            is TicketNode -> {
                icon = AllIcons.Vcs.Branch
                append(node.key + "  ", SimpleTextAttributes.REGULAR_BOLD_ATTRIBUTES)
                append(node.title)
                append("  ${node.cycles} cycles", SimpleTextAttributes.GRAYED_ATTRIBUTES)
            }
            is ProblemNode -> {
                if (node.id == UNTRACED) {
                    icon = AllIcons.General.Warning
                    append("Untraced", SimpleTextAttributes(SimpleTextAttributes.STYLE_BOLD, WishColors.UNTRACED))
                    append("  serves no problem of the ticket", SimpleTextAttributes.GRAYED_ATTRIBUTES)
                } else {
                    icon = AllIcons.Nodes.Tag
                    append(node.id + "  ", SimpleTextAttributes.REGULAR_BOLD_ATTRIBUTES)
                    append(node.text)
                }
                if (counts) append("  · ${node.count} items", SimpleTextAttributes.GRAYED_ATTRIBUTES)
            }
            is FileNode -> {
                icon = FileTypeManager.getInstance().getFileTypeByFileName(node.path.substringAfterLast('/')).icon
                append(node.path.substringAfterLast('/'))
                append("  ${node.path.substringBeforeLast('/', "")}", SimpleTextAttributes.GRAYED_ATTRIBUTES)
                if (counts) append("  · ${node.count} items", SimpleTextAttributes.GRAYED_ATTRIBUTES)
            }
            is TItem -> {
                icon = AllIcons.Nodes.Method
                append(node.item + " ")
                append(":${node.line}", SimpleTextAttributes.GRAYED_ATTRIBUTES)
            }
        }
    }
}

// ---- the Log tab ----------------------------------------------------------------------------------

/** Journal timeline on the left (like Git Log); the selected entry's files and reasoning on the right. */
class WishLogPanel(private val project: Project) : JPanel(BorderLayout()), Disposable {
    private val svc = project.service<WishService>()

    private var entries: List<Entry> = emptyList()
    private var decisions: Map<Int, Decision> = emptyMap()
    private var problems: List<ProblemText> = emptyList()
    private var changed: List<Change> = emptyList()
    private var byProblem = true

    private val tableModel = object : AbstractTableModel() {
        override fun getRowCount() = entries.size
        override fun getColumnCount() = COLUMNS.size
        override fun getColumnName(column: Int) = COLUMNS[column]
        override fun getValueAt(row: Int, column: Int): Any = entries[row]
    }
    private val table = JBTable(tableModel)
    private val browser = SimpleAsyncChangesBrowser(project, false, false)
    private val tree = Tree(DefaultTreeModel(DefaultMutableTreeNode())).apply { isRootVisible = false; showsRootHandles = true; cellRenderer = NodeRenderer() }
    private val filesCards = JPanel(CardLayout())
    private val details = JPanel().apply {
        layout = BoxLayout(this, BoxLayout.Y_AXIS)
        border = JBUI.Borders.empty(8, 12)
    }

    init {
        configureTable()
        tree.onDoubleClick { node ->
            when (node) {
                is FileNode -> changeFor(changed, node.path)?.let { showDiff(project, it, node.path, "Before", "After") }
                is TItem -> openItem(project, node)
            }
        }
        filesCards.add(browser, FOLDER)
        filesCards.add(JBScrollPane(tree).apply { border = JBUI.Borders.empty() }, PROBLEM)

        val toolbar = ActionManager.getInstance().createActionToolbar("WishLogFiles", DefaultActionGroup(GroupByProblem()), true)
        val files = JPanel(BorderLayout()).apply {
            toolbar.targetComponent = this
            add(toolbar.component, BorderLayout.NORTH)
            add(filesCards, BorderLayout.CENTER)
        }
        val right = OnePixelSplitter(true, 0.45f).apply {
            firstComponent = files
            secondComponent = JBScrollPane(details).apply { border = JBUI.Borders.empty() }
        }
        add(OnePixelSplitter(false, 0.6f).apply { firstComponent = JBScrollPane(table); secondComponent = right }, BorderLayout.CENTER)

        project.messageBus.connect(this).subscribe(WishService.CHANGED, WishListener { reload() })
        reload()
    }

    private inner class GroupByProblem : ToggleAction("Group by Problem", "Group changed files by the ticket problem they serve", AllIcons.Nodes.Tag) {
        override fun isSelected(e: AnActionEvent) = byProblem
        override fun setSelected(e: AnActionEvent, state: Boolean) {
            byProblem = state
            showFilesView()
        }

        override fun getActionUpdateThread() = ActionUpdateThread.EDT
    }

    private fun configureTable() {
        table.setDefaultRenderer(Any::class.java, object : ColoredTableCellRenderer() {
            override fun customizeCellRenderer(table: JTable, value: Any?, selected: Boolean, hasFocus: Boolean, row: Int, column: Int) {
                val entry = value as? Entry ?: return
                val byClaude = entry.author == "claude"
                when (column) {
                    0 -> {
                        icon = if (isCycle(entry)) AllIcons.Actions.Lightning else AllIcons.Actions.MenuSaveall
                        entry.ticket?.let { append("$it · ", SimpleTextAttributes.GRAYED_ATTRIBUTES) }
                        append(subject(entry), if (isCycle(entry)) SimpleTextAttributes.REGULAR_BOLD_ATTRIBUTES else SimpleTextAttributes.REGULAR_ATTRIBUTES)
                    }
                    1 -> append(if (byClaude) "claude" else "you", if (byClaude) SimpleTextAttributes.REGULAR_ATTRIBUTES else SimpleTextAttributes.GRAYED_ATTRIBUTES)
                    else -> append(entry.`when`, SimpleTextAttributes.GRAYED_ATTRIBUTES)
                }
            }
        })
        table.setShowGrid(false)
        table.tableHeader.reorderingAllowed = false
        listOf(600, 90, 140).forEachIndexed { i, w -> table.columnModel.getColumn(i).preferredWidth = w }
        table.selectionModel.selectionMode = ListSelectionModel.SINGLE_SELECTION
        table.selectionModel.addListSelectionListener { if (!it.valueIsAdjusting) show(selected()) }
    }

    private fun selected() = table.selectedRow.takeIf { it >= 0 }?.let { entries[it] }

    private fun reload() {
        svc.log().onEdt(project) { log ->
            val keep = selected()?.sha
            decisions = log.decisions.associateBy { it.id }
            problems = log.problems
            entries = log.entries
            tableModel.fireTableDataChanged()
            if (entries.isEmpty()) show(null)
            else entries.indexOfFirst { it.sha == keep }.coerceAtLeast(0).let { table.setRowSelectionInterval(it, it) }
        }
    }

    private fun show(entry: Entry?) {
        details.removeAll()
        if (entry == null) {
            browser.setChangesToDisplay(emptyList())
            tree.model = DefaultTreeModel(DefaultMutableTreeNode())
            details.add(JBLabel("No journal yet: save a file to start a cycle.").apply { foreground = UIUtil.getContextHelpForeground() })
        } else {
            addDetails(entry)
            loadChanges(entry)
        }
        details.add(Box.createVerticalGlue())
        details.revalidate()
        details.repaint()
    }

    private fun addDetails(entry: Entry) {
        details.add(html(mdInline(entry.summary.ifBlank { entry.label })))
        if (entry.why.isNotEmpty()) {
            details.add(heading("Why"))
            val rows = entry.why.joinToString("") { w ->
                val chips = w.problems.joinToString("") { "<span style='color:#6B7FD7'><small>[${esc(it)}]</small></span> " }
                "<tr><td valign='top' style='padding-right:12px'><code>${esc(w.item)}</code></td><td>$chips${mdInline(w.why)}</td></tr>"
            }
            details.add(html("<table cellpadding='2'>$rows</table>"))
        }
        if (entry.triggeredBy.isNotEmpty()) {
            details.add(heading("Triggered by your edits"))
            details.add(html(entry.triggeredBy.joinToString("<br>") { "<code>${esc(it)}</code>" }))
        }
        if (entry.decisions.isNotEmpty()) {
            details.add(heading("Decisions"))
            entry.decisions.forEach { details.add(decisionRow(it)) }
        }
    }

    private fun heading(text: String) = JBLabel(text).apply {
        font = font.deriveFont(java.awt.Font.BOLD)
        border = JBUI.Borders.empty(10, 0, 4, 0)
        alignmentX = LEFT_ALIGNMENT
    }

    private fun html(body: String) = htmlPane().apply {
        text = "<html>$body</html>"
        alignmentX = LEFT_ALIGNMENT
    }

    /** A journal decision line reads `<id> (<status>) <title>`; the live record gives status and the buttons. */
    private fun decisionRow(line: String): JComponent {
        val id = line.substringBefore(' ').toIntOrNull()
        val decision = id?.let { decisions[it] }
        return JPanel(FlowLayout(FlowLayout.LEFT, 6, 0)).apply {
            isOpaque = false
            alignmentX = LEFT_ALIGNMENT
            if (decision != null && decision.status == "proposed") {
                add(JButton("Accept").apply { addActionListener { decide(decision.id, "accept") } })
                add(JButton("Reject").apply { addActionListener { decide(decision.id, "reject") } })
            } else {
                add(JBLabel(decision?.status ?: "?").apply { foreground = UIUtil.getContextHelpForeground() })
            }
            add(JBLabel("<html><b>#${id ?: "?"} ${esc(decision?.title ?: line)}</b> · ${esc(decision?.choice)}</html>"))
            decision?.why?.let { toolTipText = it }
        }
    }

    private fun decide(id: Int, verdict: String) {
        svc.execute("wish.decide", listOf(id, verdict)).thenRun { reload() }
    }

    private fun showFilesView() {
        val grouped = byProblem && selected()?.ticket != null
        (filesCards.layout as CardLayout).show(filesCards, if (grouped) PROBLEM else FOLDER)
    }

    private fun loadChanges(entry: Entry) {
        val short = entry.sha.take(8)
        fetchChanges(project, visibleFiles(entry.files), entry.sha, null, "$short^", short).onEdt(project) { list ->
            if (selected()?.sha != entry.sha) return@onEdt
            changed = list
            browser.setChangesToDisplay(list)
            tree.model = DefaultTreeModel(logGroups(entry, problems))
            (0 until tree.rowCount).forEach { tree.expandRow(it) }
            showFilesView()
        }
    }

    override fun dispose() {}

    private fun isCycle(e: Entry) = e.label.startsWith("cycle") || e.label == "implemented"

    private fun subject(e: Entry): String {
        val first = e.summary.lineSequence().firstOrNull().orEmpty().trim().replace("`", "")
        val s = if (first.isEmpty()) e.label else "${e.label}: $first"
        return if (s.length > 90) s.take(89) + "…" else s
    }

    private companion object {
        val COLUMNS = listOf("Subject", "Author", "Date")
        const val FOLDER = "folder"
        const val PROBLEM = "problem"
    }
}
