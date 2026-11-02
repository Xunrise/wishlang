package wish

import com.intellij.icons.AllIcons
import com.intellij.openapi.Disposable
import com.intellij.openapi.actionSystem.ActionManager
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.DataContext
import com.intellij.openapi.actionSystem.ex.ComboBoxAction
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.DumbAwareAction
import javax.swing.JComponent
import com.intellij.openapi.actionSystem.AnAction
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.actionSystem.DefaultActionGroup
import com.intellij.openapi.actionSystem.Separator
import com.intellij.openapi.actionSystem.ToggleAction
import com.intellij.openapi.components.service
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.Messages
import com.intellij.openapi.vcs.changes.Change
import com.intellij.openapi.vcs.changes.ui.SimpleAsyncChangesBrowser
import com.intellij.openapi.wm.ToolWindowManager
import com.intellij.ui.OnePixelSplitter
import com.intellij.ui.components.JBLabel
import com.intellij.ui.components.JBScrollPane
import com.intellij.ui.treeStructure.Tree
import com.intellij.util.ui.JBUI
import com.intellij.util.ui.UIUtil
import java.awt.BorderLayout
import java.awt.CardLayout
import javax.swing.Icon
import javax.swing.JPanel
import javax.swing.SwingConstants
import javax.swing.event.HyperlinkEvent
import javax.swing.tree.DefaultMutableTreeNode
import javax.swing.tree.DefaultTreeModel
import javax.swing.tree.TreePath

// ---- the ticket tree ------------------------------------------------------------------------------

private val DefaultMutableTreeNode.childNodes get() = children().toList().map { it as DefaultMutableTreeNode }

/** Ticket → problems (plus [UNTRACED]) → files → items. Every problem is listed, even without code yet. */
fun ticketTree(ticket: TicketResult): DefaultMutableTreeNode {
    val root = DefaultMutableTreeNode(TicketNode(ticket.key, ticket.title.orEmpty(), ticket.cycles))
    val groups = ticket.problems.map { ProblemNode(it.id, it.text, it.items.size) to it.items } +
        listOfNotNull(ticket.untraced.takeIf { it.isNotEmpty() }?.let { ProblemNode(UNTRACED, "", it.size) to it })
    for ((problem, items) in groups) {
        val problemNode = DefaultMutableTreeNode(problem)
        for ((file, inFile) in items.groupBy { it.file }) {
            val fileNode = DefaultMutableTreeNode(FileNode(file, inFile.size))
            inFile.sortedBy { it.line }.forEach { fileNode.add(DefaultMutableTreeNode(it)) }
            problemNode.add(fileNode)
        }
        root.add(problemNode)
    }
    return root
}

private fun nodeKey(userObject: Any?) = when (userObject) {
    is TicketNode -> "ticket"
    is ProblemNode -> "p:${userObject.id}"
    is FileNode -> "f:${userObject.path}"
    is TItem -> "i:${userObject.item}"
    else -> ""
}

/** A key for a node that is stable across reloads: the keys of its path from the root. */
fun selectionKey(node: DefaultMutableTreeNode): String = node.path.joinToString("/") { nodeKey((it as DefaultMutableTreeNode).userObject) }

fun findNode(root: DefaultMutableTreeNode, matches: (DefaultMutableTreeNode) -> Boolean): DefaultMutableTreeNode? =
    root.preorderEnumeration().toList().map { it as DefaultMutableTreeNode }.firstOrNull(matches)

// ---- the Ticket tab -------------------------------------------------------------------------------

/**
 * The branch's ticket as one tree with details for the selection. With "Group by Problem" off the tree gives way to the
 * platform's changes browser over the ticket's cumulative diff.
 */
class TicketPanel(private val project: Project) : JPanel(BorderLayout()), Disposable {
    private val svc = project.service<WishService>()

    private var ticket: TicketResult? = null
    /** The ticket branch shown; null: this checkout's branch. */
    private var shownBranch: String? = null
    private var tickets: List<TicketRef> = emptyList()
    private var changed: List<Change> = emptyList()
    private var changesHead: String? = null
    private var byProblem = true
    private var shownItems: List<TItem> = emptyList()

    private val tree = Tree(DefaultTreeModel(DefaultMutableTreeNode())).apply { isRootVisible = true; showsRootHandles = true; cellRenderer = NodeRenderer(counts = true) }
    private val browser = SimpleAsyncChangesBrowser(project, false, false)
    private val leftCards = JPanel(CardLayout())
    private val details = htmlPane().apply { border = JBUI.Borders.empty(8, 14) }
    private val cards = JPanel(CardLayout())

    init {
        tree.addTreeSelectionListener { showDetails(selectedNode()) }
        tree.onDoubleClick { node ->
            when (node) {
                is FileNode -> open(node.path)
                is TItem -> open(node.file, node.line)
                is ProblemNode -> problem(node.id)?.let(::editProblem)
            }
        }
        details.addHyperlinkListener { e ->
            if (e.eventType != HyperlinkEvent.EventType.ACTIVATED) return@addHyperlinkListener
            val target = e.description
            when {
                target.startsWith("item:") -> shownItems.getOrNull(target.removePrefix("item:").toInt())?.let { open(it.file, it.line) }
                target.startsWith("file:") -> open(target.removePrefix("file:"))
                target.startsWith("diff:") -> diff(target.removePrefix("diff:"))
                target.startsWith("problem:") -> selectProblem(target.removePrefix("problem:"))
            }
        }

        leftCards.add(JBScrollPane(tree).apply { border = JBUI.Borders.empty() }, PROBLEM)
        leftCards.add(browser, FOLDER)
        val toolbar = ActionManager.getInstance().createActionToolbar("WishTicket", actions(), true)
        val left = JPanel(BorderLayout()).apply {
            toolbar.targetComponent = this
            add(toolbar.component, BorderLayout.NORTH)
            add(leftCards, BorderLayout.CENTER)
        }
        val body = OnePixelSplitter(false, 0.42f).apply {
            firstComponent = left
            secondComponent = JBScrollPane(details).apply { border = JBUI.Borders.empty() }
        }
        val empty = JBLabel("No ticket on this branch. Name the branch like feature/ABC-123-… and run protoflow start.", SwingConstants.CENTER)
            .apply { foreground = UIUtil.getContextHelpForeground() }
        cards.add(body, BODY)
        cards.add(empty, EMPTY)
        add(cards, BorderLayout.CENTER)

        project.messageBus.connect(this).subscribe(WishService.CHANGED, WishListener { reload() })
        reload()
    }

    private fun actions() = DefaultActionGroup(
        TicketPicker(), Separator(),
        GroupByProblem(), Separator(),
        simple("Open File", AllIcons.Actions.EditSource, needs = { selectedFile() != null }) { openSelected() },
        simple("Show Diff", AllIcons.Actions.Diff, needs = { selectedFile()?.let { changeFor(changed, it) } != null }) { selectedFile()?.let(::diff) },
        Separator(),
        simple("Expand All", AllIcons.Actions.Expandall) { (0 until tree.rowCount).forEach { tree.expandRow(it) } },
        simple("Collapse to Files", AllIcons.Actions.Collapseall) { collapseToFiles() },
        Separator(),
        simple("Add Problem", AllIcons.General.Add) { addProblem() },
        simple("Edit Problem", AllIcons.Actions.Edit, needsProblem = true) { selectedProblem()?.let(::editProblem) },
        simple("Remove Problem", AllIcons.General.Remove, needsProblem = true) { selectedProblem()?.let { p -> save(problems().filter { it.id != p.id }) } },
    )

    private fun simple(name: String, icon: Icon, needsProblem: Boolean = false, needs: (() -> Boolean)? = null, run: () -> Unit) = object : AnAction(name, null, icon) {
        override fun actionPerformed(e: AnActionEvent) = run()
        override fun update(e: AnActionEvent) {
            e.presentation.isEnabled = (!needsProblem || (byProblem && selectedProblem() != null)) && needs?.invoke() != false
        }

        override fun getActionUpdateThread() = ActionUpdateThread.EDT
    }

    private inner class GroupByProblem : ToggleAction("Group by Problem", "Problem → file → item; off shows the ticket's changes by folder", AllIcons.Nodes.Tag) {
        override fun isSelected(e: AnActionEvent) = byProblem
        override fun setSelected(e: AnActionEvent, state: Boolean) {
            byProblem = state
            (leftCards.layout as CardLayout).show(leftCards, if (state) PROBLEM else FOLDER)
            showDetails(if (state) selectedNode() else null)
        }

        override fun getActionUpdateThread() = ActionUpdateThread.EDT
    }

    private fun problems() = ticket?.problems.orEmpty()
    private fun problem(id: String) = problems().firstOrNull { it.id == id }
    private fun selectedNode() = tree.lastSelectedPathComponent as? DefaultMutableTreeNode

    /** The file of the selected file or item in the problem tree. */
    private fun selectedFile(): String? = if (!byProblem) null else when (val o = selectedNode()?.userObject) {
        is FileNode -> o.path
        is TItem -> o.file
        else -> null
    }

    /** An item opens at its line, a file at its top. */
    private fun openSelected() = when (val o = selectedNode()?.userObject) {
        is TItem -> open(o.file, o.line)
        is FileNode -> open(o.path)
        else -> {}
    }
    private fun root() = tree.model.root as DefaultMutableTreeNode

    /** The problem the selection sits in: the problem node itself, or a file or item below it. */
    private fun selectedProblem(): Problem? {
        var node = selectedNode()
        while (node != null) {
            (node.userObject as? ProblemNode)?.let { return problem(it.id) }
            node = node.parent as? DefaultMutableTreeNode
        }
        return null
    }

    private fun reload() {
        svc.tickets().onEdt(project) { list ->
            tickets = list
            if (shownBranch != null && list.none { it.branch == shownBranch }) shownBranch = null
            load()
        }
    }

    private fun load() {
        svc.ticket(shownBranch).onEdt(project, failed = {
            // No ticket on this branch: show another one if there is any.
            val other = tickets.firstOrNull { !it.current }
            if (shownBranch == null && other != null) show(other.branch) else (cards.layout as CardLayout).show(cards, EMPTY)
        }) { r ->
            ticket = r
            (cards.layout as CardLayout).show(cards, BODY)
            val keep = selectedNode()?.let(::selectionKey)
            tree.model = DefaultTreeModel(ticketTree(r))
            collapseToFiles()
            if (keep == null || !select { selectionKey(it) == keep }) tree.setSelectionRow(0)
            if (r.head != changesHead) loadChanges(r)
        }
    }

    /** Look at the ticket of [branch] (null or the current branch: this checkout's own). */
    private fun show(branch: String?) {
        shownBranch = branch?.takeUnless { b -> tickets.any { it.current && it.branch == b } }
        changesHead = null
        load()
    }

    /** The checkout the shown ticket's files open from (IDE path); null when no checkout has its branch. */
    private fun ticketRoot(): String? = ticket?.root?.let(svc::localPath)

    private fun open(rel: String, line: Int = 1) {
        val t = ticket ?: return
        when {
            shownBranch == null -> openFile(project, rel, line)
            ticketRoot() != null -> openFile(project, rel, line, ticketRoot())
            else -> t.head?.let { openSnapshot(project, it, rel, line, t.key) }
        }
    }

    /** Picks the ticket to look at: this branch's, or any other ticket branch with a journal. */
    private inner class TicketPicker : ComboBoxAction(), DumbAware {
        override fun createPopupActionGroup(button: JComponent, context: DataContext) = DefaultActionGroup(
            tickets.map { t ->
                object : DumbAwareAction(label(t), where(t), if (t.current) AllIcons.Vcs.Branch else null) {
                    override fun actionPerformed(e: AnActionEvent) = show(t.branch)
                }
            },
        )

        override fun update(e: AnActionEvent) {
            val t = ticket
            e.presentation.text = if (t == null) "Ticket" else t.key + (if (shownBranch == null) "" else " · ${t.branch}")
            e.presentation.description = "The ticket shown: this branch's, or another ticket branch's"
            e.presentation.isEnabled = tickets.size > 1 || (tickets.isNotEmpty() && shownBranch != null)
        }

        override fun getActionUpdateThread() = ActionUpdateThread.EDT

        private fun label(t: TicketRef) = t.key + (t.title?.let { " — ${it.take(60)}" } ?: "")

        private fun where(t: TicketRef) = when {
            t.current -> "${t.branch} (this checkout)"
            t.checkout != null -> "${t.branch} (checked out in ${t.checkout})"
            else -> "${t.branch} (not checked out: files open read-only from its journal)"
        }
    }

    private fun collapseToFiles() {
        for (row in tree.rowCount - 1 downTo 1) tree.collapseRow(row)
        tree.expandRow(0)
        var row = 0
        while (row < tree.rowCount) {
            if (tree.getPathForRow(row).pathCount == 2) tree.expandRow(row)
            row++
        }
    }

    private fun select(matches: (DefaultMutableTreeNode) -> Boolean): Boolean {
        val node = findNode(root(), matches) ?: return false
        val path = TreePath(node.path)
        tree.selectionPath = path
        tree.scrollPathToVisible(path)
        return true
    }

    fun selectProblem(id: String) {
        (leftCards.layout as CardLayout).show(leftCards, PROBLEM)
        byProblem = true
        select { (it.userObject as? ProblemNode)?.id == id }
    }

    /** The ticket's cumulative diff, from before its first cycle to its latest, as platform VCS changes. */
    private fun loadChanges(r: TicketResult) {
        changesHead = r.head
        val head = r.head ?: return
        fetchChanges(project, visibleFiles(r.files), head, r.base, "before ${r.key}", r.key).onEdt(project) {
            changed = it
            browser.setChangesToDisplay(it)
        }
    }

    private fun diff(path: String) {
        val change = changeFor(changed, path) ?: return
        val key = ticket?.key.orEmpty()
        showDiff(project, change, "$path · $key", "Before $key", "After $key")
    }

    // ---- details ----

    private fun showDetails(node: DefaultMutableTreeNode?) {
        val t = ticket ?: return
        val sb = StringBuilder("<html>")
        var items: List<TItem> = emptyList()
        when (val o = node?.userObject) {
            is ProblemNode -> if (o.id == UNTRACED) {
                sb.append("<h3><font color='#DC2626'>Untraced</font></h3>Generated in this ticket's cycles, but serving none of its problems.<br><br>")
                items = t.untraced
            } else {
                val p = problem(o.id)
                sb.append("<h3>${esc(o.id)} · ${esc(o.text)}</h3>")
                p?.scenarios.orEmpty().takeIf { it.isNotEmpty() }?.let { scenarios ->
                    sb.append("<b>Scenarios</b><br>").append(scenarios.joinToString("") { "&nbsp;&nbsp;• ${esc(it)}<br>" }).append("<br>")
                }
                items = p?.items.orEmpty()
                if (items.isEmpty()) sb.append("<font color='gray'>No generated code traces to this problem yet.</font>")
            }
            is FileNode -> {
                val under = (node.parent as? DefaultMutableTreeNode)?.userObject as? ProblemNode
                sb.append("<h3>${esc(o.path.substringAfterLast('/'))}</h3>")
                sb.append("<a href='file:${esc(o.path)}'>Open ${esc(o.path)}</a> &nbsp;·&nbsp; <a href='diff:${esc(o.path)}'>Show the ticket's diff</a><br>")
                val others = problems().filter { p -> p.id != under?.id && p.items.any { it.file == o.path } }
                if (others.isNotEmpty()) sb.append("<font color='gray'>Also serves ${others.joinToString(", ") { link("problem:${it.id}", it.id) }}</font><br>")
                sb.append("<br>")
                items = node.childNodes.mapNotNull { it.userObject as? TItem }
            }
            is TItem -> {
                val solved = problems().filter { p -> p.items.any { it.item == o.item } }
                if (solved.isNotEmpty()) {
                    sb.append("<b>Solves</b> ").append(solved.joinToString(", ") { link("problem:${it.id}", it.id) + " " + esc(it.text) }).append("<br>")
                }
                items = listOf(o)
            }
            else -> overview(sb, t)
        }
        shownItems = items
        items.forEachIndexed { i, item -> sb.append(itemHtml(i, item)) }
        details.text = sb.append("</html>").toString()
        details.caretPosition = 0
    }

    /** The ticket itself, then a summary per problem. */
    private fun overview(sb: StringBuilder, t: TicketResult) {
        sb.append(mdBlockToHtml(t.body))
        sb.append("<hr><h3>Problems extracted from the ticket</h3>")
        for (p in t.problems) {
            val files = p.items.map { it.file.substringAfterLast('/') }.distinct()
            sb.append("${link("problem:${p.id}", "<b>${esc(p.id)}</b>")} ${esc(p.text)}<br>")
            sb.append("<font color='gray'>&nbsp;&nbsp;${p.items.size} items · ${files.joinToString(", ").ifEmpty { "no code yet" }}</font><br>")
        }
        if (t.untraced.isNotEmpty()) {
            sb.append("${link("problem:$UNTRACED", "<font color='#DC2626'><b>Untraced</b></font>")} <font color='gray'>${t.untraced.size} items</font><br>")
        }
    }

    private fun link(target: String, text: String) = "<a href='$target'>$text</a>"

    private fun itemHtml(index: Int, item: TItem): String {
        val reasons = item.reasons.joinToString("") { r ->
            "<tr><td valign='top' style='padding-right:10px'><font color='gray'>${esc(r.`when`)}</font></td><td>${mdInline(r.why)}</td></tr>"
        }
        return "<p><a href='item:$index'><code>${esc(item.item)}</code></a>&nbsp;&nbsp;" +
            "<a href='file:${esc(item.file)}'><font color='gray'>${esc(item.file)}:${item.line}</font></a></p><table cellpadding='1'>$reasons</table>"
    }

    // ---- editing problems ----

    private fun editProblem(p: Problem) {
        Messages.showInputDialog(project, "Problem ${p.id}", "Edit Problem", null, p.text, null)?.takeIf { it.isNotBlank() }?.let { text ->
            save(problems().map { if (it.id == p.id) Problem(it.id, text, it.scenarios, it.items) else it })
        }
    }

    private fun addProblem() {
        Messages.showInputDialog(project, "New problem", "Add Problem", null)?.takeIf { it.isNotBlank() }?.let { text ->
            val next = (problems().mapNotNull { it.id.removePrefix("P").toIntOrNull() }.maxOrNull() ?: 0) + 1
            save(problems() + Problem("P$next", text))
        }
    }

    private fun save(all: List<Problem>) {
        val arg = all.map { mapOf("id" to it.id, "text" to it.text, "scenarios" to it.scenarios) }
        svc.execute("wish.ticket.edit", listOfNotNull(arg, shownBranch)).thenRun { reload() }
    }

    override fun dispose() {}

    private companion object {
        const val BODY = "body"
        const val EMPTY = "empty"
        const val PROBLEM = "problem"
        const val FOLDER = "folder"
    }
}

/** Enough markdown for protoflow's notes: headings, bullets, Gherkin and inline code. */
fun mdBlockToHtml(md: String): String = md.lines().joinToString("\n") { line ->
    val t = line.trimEnd()
    val bullet = t.trimStart().startsWith("- ") || t.trimStart().startsWith("* ")
    val text = mdInline(t.trimStart().removePrefix("- ").removePrefix("* "))
    when {
        t.startsWith("### ") -> "<h4>${mdInline(t.drop(4))}</h4>"
        t.startsWith("## ") -> "<h3>${mdInline(t.drop(3))}</h3>"
        t.startsWith("# ") -> "<h2>${mdInline(t.drop(2))}</h2>"
        t == "---" -> "<hr>"
        bullet -> "&nbsp;&nbsp;• $text<br>"
        Regex("^\\s*(Feature|Scenario|Given|When|Then|And|But)\\b").containsMatchIn(t) ->
            "<span style='font-family:monospace'>${"&nbsp;".repeat(t.length - t.trimStart().length)}$text</span><br>"
        t.isBlank() -> "<br>"
        else -> "$text<br>"
    }
}

/** Opens the tool window on the Ticket tab with the problem selected. */
internal fun showProblem(project: Project, id: String) {
    val toolWindow = ToolWindowManager.getInstance(project).getToolWindow(WISH_TOOL_WINDOW) ?: return
    toolWindow.activate {
        val content = toolWindow.contentManager.contents.firstOrNull { it.component is TicketPanel } ?: return@activate
        toolWindow.contentManager.setSelectedContent(content)
        (content.component as TicketPanel).selectProblem(id)
    }
}
