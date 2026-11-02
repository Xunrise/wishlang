package wish

import com.google.gson.JsonElement
import org.eclipse.lsp4j.Range
import org.eclipse.lsp4j.TextDocumentIdentifier
import org.eclipse.lsp4j.jsonrpc.services.JsonNotification
import org.eclipse.lsp4j.jsonrpc.services.JsonRequest
import org.eclipse.lsp4j.services.LanguageClient
import org.eclipse.lsp4j.services.LanguageServer
import java.util.concurrent.CompletableFuture

// Wire shapes of `wish lsp`'s custom requests. Every field has a default so Gson can build the
// classes through their no-arg constructor and absent fields fall back to empty values.

class PendingRange(val range: Range = Range(), val name: String = "")

class Lens(val line: Int = 0, val title: String = "", val command: String = "", val arguments: List<JsonElement> = emptyList())

class Block(
    val start: Int = 0, val end: Int = 0, val item: String? = null, val hover: String = "",
    val problems: List<String> = emptyList(), val untraced: Boolean = false,
)

class FileState(
    val generated: List<Range> = emptyList(), val prototype: List<Range> = emptyList(),
    val pending: List<PendingRange> = emptyList(), val lenses: List<Lens> = emptyList(),
    val blocks: List<Block> = emptyList(),
)

class Why(
    val item: String = "", val why: String = "", val problems: List<String> = emptyList(),
    val file: String? = null, val line: Int? = null,
)

class FileStat(val path: String = "", val added: Int = 0, val removed: Int = 0)

class Entry(
    val sha: String = "", val author: String = "", val `when`: String = "", val label: String = "", val summary: String = "",
    val ticket: String? = null, val why: List<Why> = emptyList(), val triggeredBy: List<String> = emptyList(),
    val decisions: List<String> = emptyList(), val mode: String = "", val files: List<FileStat> = emptyList(),
)

class Decision(val id: Int = 0, val title: String = "", val choice: String = "", val why: String = "", val status: String = "")

class ProblemText(val id: String = "", val text: String = "")

class LogResult(
    val entries: List<Entry> = emptyList(), val decisions: List<Decision> = emptyList(),
    val problems: List<ProblemText> = emptyList(), val root: String? = null,
)

class Reason(val sha: String = "", val `when`: String = "", val why: String = "")

class TItem(val item: String = "", val file: String = "", val line: Int = 1, val reasons: List<Reason> = emptyList())

class Problem(
    val id: String = "", val text: String = "", val scenarios: List<String> = emptyList(), val items: List<TItem> = emptyList(),
)

class TicketResult(
    val key: String = "", val title: String? = null, val body: String = "", val notesDir: String? = null,
    val problems: List<Problem> = emptyList(), val untraced: List<TItem> = emptyList(), val cycles: Int = 0,
    val head: String? = null, val base: String? = null, val files: List<FileStat> = emptyList(),
    /** URI of the wish project in the checkout that has the branch, which every `file` is relative to; none when no checkout has it. */
    val root: String? = null,
    val branch: String = "",
)

/** A ticket branch with a journal: one entry of the ticket picker. */
class TicketRef(
    val key: String = "", val branch: String = "", val title: String? = null, val current: Boolean = false, val checkout: String? = null,
)

class TicketParams(val branch: String? = null)

class ShowResult(val before: String = "", val after: String = "")

class FileStateParams(val textDocument: TextDocumentIdentifier)

class ShowParams(val sha: String, val path: String, val base: String? = null)

interface WishServer : LanguageServer {
    @JsonRequest("wish/fileState")
    fun fileState(params: FileStateParams): CompletableFuture<FileState>

    @JsonRequest("wish/log")
    fun log(): CompletableFuture<LogResult>

    /** Fails when the branch has no ticket key. */
    @JsonRequest("wish/ticket")
    fun ticket(params: TicketParams): CompletableFuture<TicketResult>

    /** Every ticket branch with a journal, the current one first. */
    @JsonRequest("wish/tickets")
    fun tickets(): CompletableFuture<List<TicketRef>>

    @JsonRequest("wish/show")
    fun show(params: ShowParams): CompletableFuture<ShowResult>
}

interface WishClient : LanguageClient {
    @JsonNotification("wish/changed")
    fun changed(params: Any?)
}
