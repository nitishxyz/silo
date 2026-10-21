#define _POSIX_C_SOURCE 200809L
#include <ghostty/vt.h>
#include <time.h>
#include <stdlib.h>
#include <string.h>
#include "adapter.h"

#define CHECK(call) do { int r = (call); if (r != GHOSTTY_SUCCESS) return r; } while (0)

/* All upstream layouts and enum values are compiled against pinned headers. */
struct SiloVt {
    GhosttyTerminal terminal;
    GhosttyRenderState render;
    GhosttyRenderStateRowIterator rows;
    GhosttyRenderStateRowCells cells;
    GhosttyKeyEncoder encoder;
    GhosttyMouseEncoder mouse;
    GhosttySelectionGesture selection;
    bool selecting;
    /* Shift-press extended the existing selection; drags move its end. */
    bool extending;
    /* Embedder-declared color scheme; answers CSI ? 996 n once known. */
    bool scheme_known;
    GhosttyColorScheme scheme;
    /* Lazily created by silo_vt_search_set; NULL while idle. */
    GhosttySearch search;
    /* Effects since the last silo_vt_events call. */
    uint32_t bells;
    bool title_changed;
    bool pwd_changed;
    unsigned char *clipboard[2];
    size_t clipboard_len[2];
    bool clipboard_changed[2];
    unsigned char replies[65536];
    size_t replies_len;
    bool overflow;
    /* Ordered hook events (server terminals, see silo_vt_hook): records of
     * `u32 len | u8 type | payload`, oldest first. */
    unsigned char *events;
    size_t events_len, events_cap;
    uint32_t events_count;
    bool events_lost;
    /* Incremental snapshot restore in progress; borrows `terminal`. */
    GhosttySnapshotDecoder decoder;
    unsigned char *restore_buf;
    size_t restore_len, restore_pos, restore_cap;
};

_Static_assert(sizeof(SiloCell) == 276, "Rust RawCell ABI");
_Static_assert(sizeof(SiloFrame) == 72, "Rust RawFrame ABI");
_Static_assert(sizeof(SiloLink) == 1040, "Rust RawLink ABI");
_Static_assert(sizeof(SiloEvents) == 16, "Rust RawEvents ABI");

static void write_pty(GhosttyTerminal terminal, void *userdata,
                      const uint8_t *data, size_t len) {
    (void)terminal;
    SiloVt *vt = userdata;
    if (len > sizeof(vt->replies) - vt->replies_len) {
        vt->overflow = true;
        return;
    }
    memcpy(vt->replies + vt->replies_len, data, len);
    vt->replies_len += len;
}

static bool color_scheme(GhosttyTerminal terminal, void *userdata, GhosttyColorScheme *out) {
    (void)terminal;
    SiloVt *vt = userdata;
    if (!vt->scheme_known) return false;
    *out = vt->scheme;
    return true;
}

static void bell(GhosttyTerminal terminal, void *userdata) {
    (void)terminal;
    SiloVt *vt = userdata;
    if (vt->bells < UINT32_MAX) vt->bells++;
}

static void title_changed(GhosttyTerminal terminal, void *userdata) {
    (void)terminal;
    ((SiloVt *)userdata)->title_changed = true;
}

static void clipboard_write(GhosttyTerminal terminal, void *userdata,
                            const GhosttyClipboardWrite *write) {
    (void)terminal;
    SiloVt *vt = userdata;
    GhosttyClipboardWriteResult result = GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    /* Only plain UTF-8 text can be represented by the UI clipboard bridge. */
    for (size_t i = 0; i < write->contents_len; i++) {
        GhosttyClipboardContent content = write->contents[i];
        if (content.mime.len != 10 ||
            memcmp(content.mime.ptr, "text/plain", 10) != 0) continue;
        if (content.data.len > 1024 * 1024 ||
            (content.data.len && !content.data.ptr)) {
            result = GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
            break;
        }
        unsigned char *copy = malloc(content.data.len ? content.data.len : 1);
        if (!copy) {
            result = GHOSTTY_CLIPBOARD_WRITE_RESULT_IO_ERROR;
            break;
        }
        if (content.data.len) memcpy(copy, content.data.ptr, content.data.len);
        int slot = write->location == GHOSTTY_CLIPBOARD_LOCATION_STANDARD ? 0 : 1;
        free(vt->clipboard[slot]);
        vt->clipboard[slot] = copy;
        vt->clipboard_len[slot] = content.data.len;
        vt->clipboard_changed[slot] = true;
        result = GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
        break;
    }
    GhosttyClipboardWriteReply reply = { .size = sizeof(reply), .result = result };
    write->reply(write, &reply);
}

static void pwd_changed(GhosttyTerminal terminal, void *userdata) {
    (void)terminal;
    ((SiloVt *)userdata)->pwd_changed = true;
}

/* Event queue bounds; on overflow the oldest events go and `lost` is set. */
#define EVENTS_MAX_COUNT 1024
#define EVENTS_MAX_BYTES (256 * 1024)
/* Longest string copied into an event; the embedder truncates further. */
#define EVENT_STRING_MAX 16384

enum { EV_STATUS = 1, EV_PROGRESS = 2, EV_NOTIFY = 3, EV_PROMPT = 4, EV_RESET = 5 };

typedef struct {
    unsigned char *ptr;
    size_t len, cap;
    bool failed;
} Record;

static void put(Record *r, const void *data, size_t len) {
    if (r->failed) return;
    if (r->len + len > r->cap) {
        size_t cap = r->cap ? r->cap : 256;
        while (cap < r->len + len) cap *= 2;
        unsigned char *grown = realloc(r->ptr, cap);
        if (!grown) {
            r->failed = true;
            return;
        }
        r->ptr = grown;
        r->cap = cap;
    }
    if (len) memcpy(r->ptr + r->len, data, len);
    r->len += len;
}

static void put_u8(Record *r, uint8_t v) { put(r, &v, 1); }

static void put_string(Record *r, GhosttyString s) {
    uint32_t len = (uint32_t)(s.ptr ? (s.len < EVENT_STRING_MAX ? s.len : EVENT_STRING_MAX) : 0);
    put(r, &len, 4);
    put(r, s.ptr, len);
}

static Record record(uint8_t type) {
    Record r = {0};
    uint32_t len = 0;
    put(&r, &len, 4);
    put_u8(&r, type);
    return r;
}

/* A prompt record owns the tracked ref stored right after its type. */
static void release_record(const unsigned char *rec) {
    if (rec[4] != EV_PROMPT) return;
    GhosttyTrackedGridRef ref;
    memcpy(&ref, rec + 5, sizeof(ref));
    ghostty_tracked_grid_ref_free(ref);
}

static void drop_oldest(SiloVt *vt) {
    uint32_t len;
    memcpy(&len, vt->events, 4);
    release_record(vt->events);
    memmove(vt->events, vt->events + len, vt->events_len - len);
    vt->events_len -= len;
    vt->events_count--;
    vt->events_lost = true;
}

static void push(SiloVt *vt, Record *r) {
    if (!r->failed && r->len <= EVENTS_MAX_BYTES) {
        uint32_t len = (uint32_t)r->len;
        memcpy(r->ptr, &len, 4);
        while (vt->events_count && (vt->events_count >= EVENTS_MAX_COUNT || vt->events_len + r->len > EVENTS_MAX_BYTES))
            drop_oldest(vt);
        if (vt->events_len + r->len > vt->events_cap) {
            size_t cap = vt->events_cap ? vt->events_cap : 4096;
            while (cap < vt->events_len + r->len) cap *= 2;
            unsigned char *grown = realloc(vt->events, cap);
            if (grown) {
                vt->events = grown;
                vt->events_cap = cap;
            } else {
                r->failed = true;
            }
        }
    } else {
        r->failed = true;
    }
    if (r->failed) {
        if (r->ptr && r->len >= 5 + sizeof(GhosttyTrackedGridRef)) release_record(r->ptr);
        vt->events_lost = true;
    } else {
        memcpy(vt->events + vt->events_len, r->ptr, r->len);
        vt->events_len += r->len;
        vt->events_count++;
    }
    free(r->ptr);
}

static void program_status(GhosttyTerminal terminal, void *userdata, const GhosttyTerminalProgramStatus *report) {
    (void)terminal;
    Record r = record(EV_STATUS);
    put_u8(&r, (uint8_t)report->state);
    put_u8(&r, (uint8_t)report->kind);
    put(&r, &report->progress, 1);
    put_string(&r, report->id);
    put_string(&r, report->app);
    put_string(&r, report->title);
    put_string(&r, report->message);
    push(userdata, &r);
}

static void progress_report(GhosttyTerminal terminal, void *userdata, const GhosttyTerminalProgressReport *report) {
    (void)terminal;
    Record r = record(EV_PROGRESS);
    put_u8(&r, (uint8_t)report->state);
    put(&r, &report->progress, 1);
    push(userdata, &r);
}

static void desktop_notification(GhosttyTerminal terminal, void *userdata, const GhosttyTerminalDesktopNotification *n) {
    (void)terminal;
    Record r = record(EV_NOTIFY);
    put_string(&r, n->title);
    put_string(&r, n->body);
    push(userdata, &r);
}

/* Tracks the cursor's cell: the terminal has already applied the mark.
 * Primary screen only: anchors are counted on it (silo_vt_row_from_bottom). */
static GhosttyTrackedGridRef track_cursor(GhosttyTerminal terminal) {
    GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
    if (ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen) != GHOSTTY_SUCCESS) return NULL;
    if (screen != GHOSTTY_TERMINAL_SCREEN_PRIMARY) return NULL;
    uint16_t x = 0, y = 0;
    if (ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_CURSOR_X, &x) != GHOSTTY_SUCCESS) return NULL;
    if (ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_CURSOR_Y, &y) != GHOSTTY_SUCCESS) return NULL;
    GhosttyPoint at = { .tag = GHOSTTY_POINT_TAG_ACTIVE, .value.coordinate = { .x = x, .y = y } };
    GhosttyTrackedGridRef ref = NULL;
    if (ghostty_terminal_grid_ref_track(terminal, at, &ref) != GHOSTTY_SUCCESS) return NULL;
    return ref;
}

static void semantic_prompt(GhosttyTerminal terminal, void *userdata, const GhosttyTerminalSemanticPrompt *event) {
    Record r = record(EV_PROMPT);
    GhosttyTrackedGridRef ref = NULL;
    if (event->kind == GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START || event->kind == GHOSTTY_SEMANTIC_PROMPT_COMMAND_END)
        ref = track_cursor(terminal);
    put(&r, &ref, sizeof(ref));
    if (r.failed) ghostty_tracked_grid_ref_free(ref);
    put_u8(&r, (uint8_t)event->kind);
    put_u8(&r, (uint8_t)event->prompt_kind);
    put_u8(&r, event->has_exit_code ? 1 : 0);
    put(&r, &event->exit_code, 4);
    put_string(&r, event->command);
    put_string(&r, event->error);
    push(userdata, &r);
}

static void reset(GhosttyTerminal terminal, void *userdata) {
    (void)terminal;
    Record r = record(EV_RESET);
    push(userdata, &r);
}

int silo_vt_hook(SiloVt *vt) {
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_PROGRAM_STATUS, program_status));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT, progress_report));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION, desktop_notification));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT, semantic_prompt));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_RESET, reset));
    return 0;
}

size_t silo_vt_take_events(SiloVt *vt, unsigned char *out, size_t cap, bool *lost) {
    size_t len = vt->events_len;
    *lost = vt->events_lost;
    if (len > cap) return len;
    if (len) memcpy(out, vt->events, len);
    vt->events_len = 0;
    vt->events_count = 0;
    vt->events_lost = false;
    return len;
}

void silo_tracked_free(void *ref) {
    ghostty_tracked_grid_ref_free(ref);
}

/* Rows below `ref`'s row in its screen (primary only: an anchor on the
 * primary screen can't be placed while the alternate one is active). */
int silo_vt_row_from_bottom(SiloVt *vt, void *ref, uint32_t *out) {
    GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen));
    if (screen != GHOSTTY_TERMINAL_SCREEN_PRIMARY) return GHOSTTY_NO_VALUE;
    GhosttyPointCoordinate at;
    CHECK(ghostty_tracked_grid_ref_point(ref, GHOSTTY_POINT_TAG_SCREEN, &at));
    size_t total = 0;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_TOTAL_ROWS, &total));
    if (at.y >= total) return GHOSTTY_NO_VALUE;
    *out = (uint32_t)(total - 1 - at.y);
    return 0;
}

/* Tracks column `x` of the row `from_bottom` rows above the primary
 * screen's last row. */
int silo_vt_track_row(SiloVt *vt, uint32_t from_bottom, uint16_t x, void **out) {
    *out = NULL;
    GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen));
    if (screen != GHOSTTY_TERMINAL_SCREEN_PRIMARY) return GHOSTTY_NO_VALUE;
    size_t total = 0;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_TOTAL_ROWS, &total));
    if (from_bottom >= total) return GHOSTTY_NO_VALUE;
    GhosttyPoint at = { .tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = { .x = x, .y = (uint32_t)(total - 1 - from_bottom) } };
    GhosttyTrackedGridRef ref = NULL;
    CHECK(ghostty_terminal_grid_ref_track(vt->terminal, at, &ref));
    *out = ref;
    return 0;
}

/* Column of `ref` (to re-pin it after a restore). */
int silo_vt_tracked_column(void *ref, uint16_t *x) {
    GhosttyPointCoordinate at;
    CHECK(ghostty_tracked_grid_ref_point(ref, GHOSTTY_POINT_TAG_SCREEN, &at));
    *x = at.x;
    return 0;
}

/* Plain text of the command output containing `ref`, cut before `end`
 * (the command's D mark) when given; empty when the cell is not command
 * output. Allocated with ghostty's allocator. */
int silo_vt_output_text(SiloVt *vt, void *ref, void *end, uint8_t **out, size_t *len, bool *cut) {
    *out = NULL;
    *len = 0;
    *cut = false;
    /* Refs live on the primary screen; selection and formatting use the
     * active one, so they must be the same page list. */
    GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen));
    if (screen != GHOSTTY_TERMINAL_SCREEN_PRIMARY) return 0;
    GhosttyGridRef at = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    GhosttySelection output = GHOSTTY_INIT_SIZED(GhosttySelection);
    int res = ghostty_tracked_grid_ref_snapshot(ref, &at);
    if (res == GHOSTTY_NO_VALUE) {
        /* The start was pruned from the scrollback. Everything left above
         * the end is this command's output: from the top to the end. */
        *cut = true;
        if (!end || !ghostty_tracked_grid_ref_has_value(end)) return 0;
        GhosttyPoint top = { .tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = { .x = 0, .y = 0 } };
        CHECK(ghostty_terminal_grid_ref(vt->terminal, top, &output.start));
        CHECK(ghostty_tracked_grid_ref_snapshot(end, &output.end));
    } else {
        CHECK(res);
        res = ghostty_terminal_select_output(vt->terminal, at, &output);
        if (res == GHOSTTY_NO_VALUE) return 0;
        CHECK(res);
    }
    GhosttyPointCoordinate stop, start, last;
    if (end && ghostty_tracked_grid_ref_point(end, GHOSTTY_POINT_TAG_SCREEN, &stop) == GHOSTTY_SUCCESS &&
        ghostty_terminal_point_from_grid_ref(vt->terminal, &output.start, GHOSTTY_POINT_TAG_SCREEN, &start) == GHOSTTY_SUCCESS &&
        ghostty_terminal_point_from_grid_ref(vt->terminal, &output.end, GHOSTTY_POINT_TAG_SCREEN, &last) == GHOSTTY_SUCCESS &&
        (last.y > stop.y || (last.y == stop.y && last.x >= stop.x))) {
        /* The output ends at the cell before D. */
        GhosttyPointCoordinate cut = stop;
        if (cut.x > 0) {
            cut.x--;
        } else if (cut.y > 0) {
            uint16_t cols = 0;
            CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols));
            cut.y--;
            cut.x = cols ? cols - 1 : 0;
        } else {
            return 0;
        }
        if (cut.y < start.y || (cut.y == start.y && cut.x < start.x)) return 0;
        GhosttyPoint point = { .tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = cut };
        CHECK(ghostty_terminal_grid_ref(vt->terminal, point, &output.end));
    }
    GhosttyTerminalSelectionFormatOptions options = GHOSTTY_INIT_SIZED(GhosttyTerminalSelectionFormatOptions);
    options.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
    options.unwrap = true;
    options.trim = true;
    options.selection = &output;
    res = ghostty_terminal_selection_format_alloc(vt->terminal, NULL, options, out, len);
    return res == GHOSTTY_NO_VALUE ? 0 : res;
}

int silo_vt_tracked_viewport_row(SiloVt *vt, void *ref, uint16_t *y) {
    if (!ghostty_tracked_grid_ref_has_value(ref)) return GHOSTTY_NO_VALUE;
    GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen));
    if (screen != GHOSTTY_TERMINAL_SCREEN_PRIMARY) return 3;
    GhosttyPointCoordinate at;
    /* No viewport point for a row above the viewport. */
    if (ghostty_tracked_grid_ref_point(ref, GHOSTTY_POINT_TAG_VIEWPORT, &at) != GHOSTTY_SUCCESS) return 2;
    uint16_t rows = 0;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_ROWS, &rows));
    if (at.y >= rows) return 1;
    *y = (uint16_t)at.y;
    return 0;
}

/* Ghostty's output selection grows from any output cell to the whole
 * output after its prompt, ending before the next prompt row (the D row,
 * where the next prompt starts), so the last cell above D finds it. */
int silo_vt_select_output_before(SiloVt *vt, void *ref, uint32_t *start_y) {
    GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen));
    if (screen != GHOSTTY_TERMINAL_SCREEN_PRIMARY) return GHOSTTY_NO_VALUE;
    GhosttyPointCoordinate end;
    int res = ghostty_tracked_grid_ref_point(ref, GHOSTTY_POINT_TAG_SCREEN, &end);
    if (res != GHOSTTY_SUCCESS) return res;
    if (end.y == 0) return GHOSTTY_NO_VALUE;
    uint16_t cols = 0;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols));
    GhosttyPoint above = { .tag = GHOSTTY_POINT_TAG_SCREEN, .value.coordinate = { .x = cols ? cols - 1 : 0, .y = end.y - 1 } };
    GhosttyGridRef at = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    CHECK(ghostty_terminal_grid_ref(vt->terminal, above, &at));
    GhosttySelection output = GHOSTTY_INIT_SIZED(GhosttySelection);
    res = ghostty_terminal_select_output(vt->terminal, at, &output);
    if (res != GHOSTTY_SUCCESS) return res;
    GhosttyPointCoordinate start;
    CHECK(ghostty_terminal_point_from_grid_ref(vt->terminal, &output.start, GHOSTTY_POINT_TAG_SCREEN, &start));
    *start_y = start.y;
    vt->selecting = false;
    vt->extending = false;
    ghostty_selection_gesture_reset(vt->selection, vt->terminal);
    return ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, &output);
}

void silo_vt_free(SiloVt *vt) {
    if (!vt) return;
    for (size_t at = 0; at < vt->events_len;) {
        uint32_t len;
        memcpy(&len, vt->events + at, 4);
        release_record(vt->events + at);
        at += len;
    }
    free(vt->events);
    /* The decoder borrows the terminal; release it first. */
    ghostty_snapshot_decoder_free(vt->decoder);
    free(vt->restore_buf);
    /* The search borrows the terminal; release its tracked state first. */
    ghostty_search_free(vt->search);
    ghostty_key_encoder_free(vt->encoder);
    ghostty_mouse_encoder_free(vt->mouse);
    ghostty_selection_gesture_free(vt->selection, vt->terminal);
    ghostty_render_state_row_cells_free(vt->cells);
    ghostty_render_state_row_iterator_free(vt->rows);
    ghostty_render_state_free(vt->render);
    ghostty_terminal_free(vt->terminal);
    free(vt->clipboard[0]);
    free(vt->clipboard[1]);
    free(vt);
}


/* Wires render state, encoders and callbacks around vt->terminal. Options
 * that a snapshot carries (scrollback limits, modes, colors) are left alone. */
static int attach(SiloVt *vt) {
    CHECK(ghostty_render_state_new(NULL, &vt->render));
    CHECK(ghostty_render_state_row_iterator_new(NULL, &vt->rows));
    CHECK(ghostty_render_state_row_cells_new(NULL, &vt->cells));
    CHECK(ghostty_key_encoder_new(NULL, &vt->encoder));
    CHECK(ghostty_mouse_encoder_new(NULL, &vt->mouse));
    CHECK(ghostty_selection_gesture_new(NULL, &vt->selection));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_USERDATA, vt));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_WRITE_PTY, write_pty));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_COLOR_SCHEME, color_scheme));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_BELL, bell));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, title_changed));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_PWD_CHANGED, pwd_changed));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE, clipboard_write));
    size_t apc = 65536, zero = 0;
    bool disabled = false;
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_APC_MAX_BYTES, &apc));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_UNKNOWN_MAX_BYTES, &zero));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_GLYPH_PROTOCOL, &disabled));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE, &disabled));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_TEMP_FILE, NULL));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_SHARED_MEM, &disabled));
    return 0;
}

static int configure(SiloVt *vt, uint16_t columns, uint16_t rows, size_t continuation_max) {
    CHECK(ghostty_terminal_new(NULL, &vt->terminal, columns, rows));
    /* Continuation tracking must be enabled before any input so a snapshot
     * taken mid-escape-sequence can be encoded. */
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES, &continuation_max));
    size_t scrollback = 16 * 1024 * 1024, lines = 10000;
    /* Upstream's built-in default is a steady cursor; silo's default is
     * blinking. DECSCUSR steady/blinking selections still override this, and
     * CSI 0 SP q / RIS restore it. */
    bool blink = true;
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_BLINK, &blink));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, &scrollback));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &lines));
    return attach(vt);
}

SiloVt *silo_vt_new(uint16_t columns, uint16_t rows, size_t continuation_max) {
    SiloVt *vt = calloc(1, sizeof(*vt));
    if (vt && configure(vt, columns, rows, continuation_max) != 0) {
        silo_vt_free(vt);
        return NULL;
    }
    return vt;
}

int silo_vt_size(SiloVt *vt, uint32_t *out) {
    uint16_t cols = 0, rows = 0;
    uint32_t width = 0, height = 0;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_COLS, &cols));
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_ROWS, &rows));
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_WIDTH_PX, &width));
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_HEIGHT_PX, &height));
    out[0] = cols;
    out[1] = rows;
    out[2] = width;
    out[3] = height;
    return 0;
}

int silo_vt_ground(SiloVt *vt, bool *ground) {
    return ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_VT_GROUND, ground);
}

int silo_vt_snapshot(SiloVt *vt, uint8_t **out, size_t *len) {
    return ghostty_snapshot_encode_alloc(vt->terminal, NULL, out, len);
}

void silo_bytes_free(uint8_t *ptr, size_t len) {
    ghostty_free(NULL, ptr, len);
}

/* The decoder reads from vt->restore_buf. The Rust side only calls ready/next
 * once the records they consume are fully buffered, so a short buffer here is
 * a protocol bug and reported as end-of-file (truncated snapshot). */
static bool restore_read(void *userdata, uint8_t *buffer, size_t capacity, size_t *out_read) {
    SiloVt *vt = userdata;
    size_t available = vt->restore_len - vt->restore_pos;
    size_t count = available < capacity ? available : capacity;
    if (count) memcpy(buffer, vt->restore_buf + vt->restore_pos, count);
    vt->restore_pos += count;
    *out_read = count;
    return true;
}

static int restore_push(SiloVt *vt, const uint8_t *data, size_t len) {
    if (vt->restore_pos == vt->restore_len) vt->restore_pos = vt->restore_len = 0;
    if (vt->restore_pos > 0 && vt->restore_len + len > vt->restore_cap) {
        memmove(vt->restore_buf, vt->restore_buf + vt->restore_pos, vt->restore_len - vt->restore_pos);
        vt->restore_len -= vt->restore_pos;
        vt->restore_pos = 0;
    }
    if (vt->restore_len + len > vt->restore_cap) {
        size_t cap = vt->restore_cap ? vt->restore_cap : 65536;
        while (cap < vt->restore_len + len) cap *= 2;
        unsigned char *grown = realloc(vt->restore_buf, cap);
        if (!grown) return GHOSTTY_OUT_OF_MEMORY;
        vt->restore_buf = grown;
        vt->restore_cap = cap;
    }
    memcpy(vt->restore_buf + vt->restore_len, data, len);
    vt->restore_len += len;
    return 0;
}

/* Decodes a snapshot prefix through READY. `head` must contain the envelope
 * and every record up to and including READY. History is restored later via
 * silo_vt_restore_push + silo_vt_restore_next. */
SiloVt *silo_vt_restore(const uint8_t *head, size_t len, bool retain_continuation,
                        bool compress_history, uint64_t *history_rows, int *error) {
    SiloVt *vt = calloc(1, sizeof(*vt));
    *error = GHOSTTY_OUT_OF_MEMORY;
    if (!vt) return NULL;
    int r = restore_push(vt, head, len);
    GhosttyReader reader = { .read = restore_read, .userdata = vt };
    if (!r) r = ghostty_snapshot_decoder_new(NULL, &vt->decoder, reader);
    if (!r) r = ghostty_snapshot_decoder_set(vt->decoder, GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION, &retain_continuation);
    if (!r) r = ghostty_snapshot_decoder_set(vt->decoder, GHOSTTY_SNAPSHOT_DECODER_OPT_COMPRESS_HISTORY, &compress_history);
    if (!r) r = ghostty_snapshot_decoder_ready(vt->decoder, &vt->terminal);
    *history_rows = 0;
    if (!r) {
        uint64_t rows = 0;
        if (ghostty_snapshot_decoder_get(vt->decoder, GHOSTTY_SNAPSHOT_DECODER_DATA_HISTORY_ROWS_PRIMARY, &rows) == GHOSTTY_SUCCESS)
            *history_rows = rows;
    }
    if (!r) r = attach(vt);
    if (r) {
        *error = r;
        silo_vt_free(vt);
        return NULL;
    }
    *error = 0;
    return vt;
}

int silo_vt_restore_push(SiloVt *vt, const uint8_t *data, size_t len) {
    if (!vt->decoder) return GHOSTTY_INVALID_VALUE;
    return restore_push(vt, data, len);
}

/* 0: one page restored (rows prepended in *rows); 1: FINISH validated and
 * the decoder released; otherwise an upstream error (decoder released). */
int silo_vt_restore_next(SiloVt *vt, size_t *rows) {
    *rows = 0;
    if (!vt->decoder) return 1;
    int r = ghostty_snapshot_decoder_next(vt->decoder);
    if (r == GHOSTTY_SUCCESS) {
        ghostty_snapshot_decoder_get(vt->decoder, GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_ROWS, rows);
        return 0;
    }
    ghostty_snapshot_decoder_free(vt->decoder);
    vt->decoder = NULL;
    free(vt->restore_buf);
    vt->restore_buf = NULL;
    vt->restore_len = vt->restore_pos = vt->restore_cap = 0;
    return r == GHOSTTY_NO_VALUE ? 1 : (r ? r : -1);
}

void silo_vt_reset(SiloVt *vt) {
    ghostty_terminal_reset(vt->terminal);
}

int silo_vt_write(SiloVt *vt, const unsigned char *data, size_t len) {
    ghostty_terminal_vt_write(vt->terminal, data, len);
    return vt->overflow ? -1 : 0;
}

size_t silo_vt_replies(SiloVt *vt, unsigned char *out) {
    size_t len = vt->replies_len;
    memcpy(out, vt->replies, len);
    vt->replies_len = 0;
    return len;
}

int silo_vt_resize(SiloVt *vt, uint16_t columns, uint16_t rows,
                       uint16_t width, uint16_t height) {
    return ghostty_terminal_resize(vt->terminal, columns, rows, width, height);
}

static uint32_t rgb(GhosttyColorRgb c) {
    return ((uint32_t)c.r << 16) | ((uint32_t)c.g << 8) | c.b;
}

static GhosttyColorRgb color_from(uint32_t packed) {
    GhosttyColorRgb c = { .r = (packed >> 16) & 0xff, .g = (packed >> 8) & 0xff, .b = packed & 0xff };
    return c;
}

/* Installs the embedder's theme as Ghostty's *defaults*. Upstream keeps any
 * OSC 10/11/12 override in force and `changeDefault` only rewrites palette
 * indices a program has not overridden with OSC 4, so explicit program
 * colors survive both the initial install and a later theme switch. The
 * 216-color cube and grayscale ramp are regenerated upstream from the theme
 * background/foreground; `harmonious = false` keeps them dark-to-light on a
 * light theme, as programs expect. */
int silo_vt_set_colors(SiloVt *vt, const uint32_t *colors, bool dark,
                           char *out, size_t cap, size_t *written) {
    GhosttyColorRgb fg = color_from(colors[0]);
    GhosttyColorRgb bg = color_from(colors[1]);
    GhosttyColorRgb cursor = color_from(colors[2]);
    GhosttyColorRgb palette[256];
    ghostty_color_palette_default(palette);
    for (size_t i = 0; i < 16; ++i) palette[i] = color_from(colors[3 + i]);
    ghostty_color_palette_generate(palette, NULL, &bg, &fg, false, palette);
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, palette));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &fg));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &bg));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &cursor));
    GhosttyColorScheme scheme = dark ? GHOSTTY_COLOR_SCHEME_DARK : GHOSTTY_COLOR_SCHEME_LIGHT;
    bool changed = !vt->scheme_known || vt->scheme != scheme;
    vt->scheme_known = true;
    vt->scheme = scheme;
    *written = 0;
    /* Unsolicited scheme reports only when the program opted in (mode 2031)
     * and the scheme actually changed. */
    if (!changed) return 0;
    GhosttyTerminalModeConfig mode = { .mode = GHOSTTY_MODE_COLOR_SCHEME_REPORT };
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_MODE, &mode));
    if (!mode.value) return 0;
    return ghostty_color_scheme_report_encode(scheme, out, cap, written);
}

/* Defaults the user configured in terminal.conf. The cursor style and blink
 * are installed as *defaults* (what DECSCUSR reset restores), exactly like the
 * theme colors above, so a program that selects its own cursor keeps it. The
 * scrollback bounds replace the built-in ones; upstream prunes existing
 * history to the new bound on its own schedule. */
int silo_vt_set_defaults(SiloVt *vt, int32_t cursor_style, int32_t cursor_blink,
                             uint64_t scrollback_bytes, uint64_t scrollback_lines) {
    GhosttyTerminalCursorStyle style = (GhosttyTerminalCursorStyle)(cursor_style < 0 ? 0 : cursor_style);
    /* A NULL value restores upstream's built-in default, so removing a key
     * from terminal.conf and saving really does go back to the default. */
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_STYLE,
                               cursor_style < 0 ? NULL : &style));
    bool blink = cursor_blink > 0;
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_BLINK,
                               cursor_blink < 0 ? NULL : &blink));
    size_t bytes = (size_t)scrollback_bytes;
    size_t lines = (size_t)scrollback_lines;
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, &bytes));
    CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &lines));
    return 0;
}

int silo_vt_scrollbar(SiloVt *vt, uint64_t *out) {
    GhosttyTerminalScrollbar scrollbar = {0};
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &scrollbar));
    out[0] = scrollbar.total;
    out[1] = scrollbar.offset;
    out[2] = scrollbar.len;
    return 0;
}

void silo_vt_events(SiloVt *vt, SiloEvents *out) {
    out->bells = vt->bells;
    out->title_changed = vt->title_changed;
    out->pwd_changed = vt->pwd_changed;
    out->reserved = (vt->clipboard_changed[0] ? 1 : 0) |
                    (vt->clipboard_changed[1] ? 2 : 0);
    vt->bells = 0;
    vt->title_changed = false;
    vt->pwd_changed = false;
}

size_t silo_vt_clipboard(SiloVt *vt, int slot, unsigned char *out, size_t cap) {
    if (slot < 0 || slot > 1) return 0;
    size_t len = vt->clipboard_len[slot];
    if (len > cap) return len;
    if (len) memcpy(out, vt->clipboard[slot], len);
    free(vt->clipboard[slot]);
    vt->clipboard[slot] = NULL;
    vt->clipboard_len[slot] = 0;
    vt->clipboard_changed[slot] = false;
    return len;
}

int silo_vt_string(SiloVt *vt, int which, unsigned char *out, size_t cap, size_t *written) {
    GhosttyString value = { .ptr = NULL, .len = 0 };
    CHECK(ghostty_terminal_get(vt->terminal,
        which == 0 ? GHOSTTY_TERMINAL_DATA_TITLE : GHOSTTY_TERMINAL_DATA_PWD, &value));
    size_t len = value.len < cap ? value.len : cap;
    if (len && value.ptr) memcpy(out, value.ptr, len);
    *written = len;
    return 0;
}

/* Hyperlink spans are collected per extracted row; adjacent cells with the
 * same URI merge into one span. Collection stops silently at link_cap. */
typedef struct {
    SiloLink *links;
    size_t cap;
    size_t count;
} LinkSink;

static void collect_link(SiloVt *vt, LinkSink *sink, size_t row, size_t column) {
    GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    GhosttyPoint point = { .tag = GHOSTTY_POINT_TAG_VIEWPORT,
        .value.coordinate = { .x = (uint16_t)column, .y = (uint32_t)row } };
    if (ghostty_terminal_grid_ref(vt->terminal, point, &ref) != GHOSTTY_SUCCESS) return;
    unsigned char uri[1024];
    size_t len = 0;
    int result = ghostty_grid_ref_hyperlink_uri(&ref, uri, sizeof(uri), &len);
    /* Oversized URIs are dropped rather than truncated into a different target. */
    if (result != GHOSTTY_SUCCESS || len == 0) return;
    if (sink->count > 0) {
        SiloLink *last = &sink->links[sink->count - 1];
        if (last->row == row && last->end == column && last->len == len &&
            memcmp(last->uri, uri, len) == 0) {
            last->end = (uint32_t)column + 1;
            return;
        }
    }
    if (sink->count >= sink->cap) return;
    SiloLink *link = &sink->links[sink->count++];
    link->row = (uint32_t)row;
    link->start = (uint32_t)column;
    link->end = (uint32_t)column + 1;
    link->len = (uint32_t)len;
    memcpy(link->uri, uri, len);
}

static int extract_row(SiloVt *vt, const GhosttyRenderStateColors *colors,
                       SiloCell *cells, size_t columns, size_t row, LinkSink *sink) {
    CHECK(ghostty_render_state_row_get(vt->rows, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, &vt->cells));
    size_t i = 0;
    while (ghostty_render_state_row_cells_next(vt->cells)) {
        if (i >= columns) return -1;
        SiloCell *cell = &cells[i++];
        GhosttyBuffer text = { .ptr = cell->text, .cap = sizeof(cell->text), .len = 0 };
        CHECK(ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8, &text));
        cell->len = (uint32_t)text.len;
        GhosttyStyle style = GHOSTTY_INIT_SIZED(GhosttyStyle);
        CHECK(ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, &style));
        GhosttyColorRgb fg = colors->foreground, bg = colors->background;
        ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR, &fg);
        bool explicit_bg = ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR, &bg) == GHOSTTY_SUCCESS;
        cell->foreground = rgb(style.inverse ? bg : fg);
        cell->background = rgb(style.inverse ? fg : bg);
        bool selected = false;
        CHECK(ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_SELECTED, &selected));
        if (style.invisible) cell->len = 0;
        /* Selection is reported as a flag only; the renderer paints the
         * theme's translucent selection over the cell's own colors. */
        GhosttyCell raw;
        GhosttyCellWide wide;
        CHECK(ghostty_render_state_row_cells_get(vt->cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &raw));
        CHECK(ghostty_cell_get(raw, GHOSTTY_CELL_DATA_WIDE, &wide));
        cell->width = wide == GHOSTTY_CELL_WIDE_WIDE ? 2 :
            (wide == GHOSTTY_CELL_WIDE_NARROW ? 1 : 0);
        bool hyperlink = false;
        CHECK(ghostty_cell_get(raw, GHOSTTY_CELL_DATA_HAS_HYPERLINK, &hyperlink));
        cell->flags = style.bold | (style.italic << 1) | (style.faint << 2) |
            ((style.underline != 0) << 3) | (style.strikethrough << 4) |
            ((explicit_bg || style.inverse) << 5) | (selected << 6) | (hyperlink << 7);
        if (hyperlink) collect_link(vt, sink, row, i - 1);
    }
    return i == columns ? 0 : -1;
}

int silo_vt_frame(SiloVt *vt, SiloFrame *out, SiloCell *cells, size_t capacity,
                      uint16_t columns, bool full, uint64_t *dirty_rows,
                      SiloLink *links, size_t link_cap, size_t *link_count) {
    LinkSink sink = { .links = links, .cap = link_cap, .count = 0 };
    *link_count = 0;
    CHECK(ghostty_render_state_begin_update(vt->render, vt->terminal));
    CHECK(ghostty_render_state_end_update(vt->render));
    uint64_t scrollbar[3];
    CHECK(silo_vt_scrollbar(vt, scrollbar));
    out->scroll_total = scrollbar[0];
    out->scroll_offset = scrollbar[1];
    out->scroll_len = scrollbar[2];
    bool tracking = false;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, &tracking));
    out->mouse_tracking = tracking;
    bool at_prompt = false;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_CURSOR_AT_PROMPT, &at_prompt));
    out->at_prompt = at_prompt;
    GhosttyTerminalScreen screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen));
    out->alt_screen = screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE;
    GhosttyTerminalModeConfig sync = { .mode = GHOSTTY_MODE_SYNC_OUTPUT };
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_MODE, &sync));
    out->synchronized = sync.value;
    GhosttyRenderStateColors colors = GHOSTTY_INIT_SIZED(GhosttyRenderStateColors);
    GhosttyRenderStateCursor cursor = GHOSTTY_INIT_SIZED(GhosttyRenderStateCursor);
    CHECK(ghostty_render_state_get(vt->render, GHOSTTY_RENDER_STATE_DATA_COLORS, &colors));
    CHECK(ghostty_render_state_get(vt->render, GHOSTTY_RENDER_STATE_DATA_CURSOR, &cursor));
    out->background = rgb(colors.background);
    out->cursor_x = cursor.viewport_has_value ? cursor.viewport_x : 0;
    out->cursor_y = cursor.viewport_has_value ? cursor.viewport_y : 0;
    out->cursor_visible = cursor.visible && cursor.viewport_has_value;
    out->cursor_blink = cursor.blinking;
    out->cursor_color = rgb(colors.cursor_has_value ? colors.cursor : colors.foreground);
    switch (cursor.visual_style) {
        case GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR: out->cursor_shape = 0; break;
        case GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK: out->cursor_shape = 1; break;
        case GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE: out->cursor_shape = 2; break;
        case GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW: out->cursor_shape = 3; break;
        default: return -1;
    }
    /* Upstream tracks a global clean/partial/full state plus per-row dirty
     * flags; both are consumed by ghostty_render_state_clean below, so a
     * frame that fails midway leaves them set for the next attempt. */
    GhosttyRenderStateDirty dirty = GHOSTTY_RENDER_STATE_DIRTY_FULL;
    CHECK(ghostty_render_state_get(vt->render, GHOSTTY_RENDER_STATE_DATA_DIRTY, &dirty));
    bool extract_all = full || dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL;
    memset(dirty_rows, 0, 4 * sizeof(uint64_t));
    out->dirty = extract_all ? 2 : (dirty == GHOSTTY_RENDER_STATE_DIRTY_FALSE ? 0 : 1);
    if (out->dirty == 0) return ghostty_render_state_clean(vt->render);
    CHECK(ghostty_render_state_get(vt->render, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &vt->rows));
    size_t row = 0;
    while (ghostty_render_state_row_iterator_next(vt->rows)) {
        if ((row + 1) * columns > capacity || row >= 256) return -1;
        bool row_dirty = extract_all;
        if (!row_dirty) {
            CHECK(ghostty_render_state_row_get(vt->rows, GHOSTTY_RENDER_STATE_ROW_DATA_DIRTY, &row_dirty));
        }
        if (row_dirty) {
            dirty_rows[row / 64] |= UINT64_C(1) << (row % 64);
            CHECK(extract_row(vt, &colors, cells + row * columns, columns, row, &sink));
        }
        row++;
    }
    if (row * columns != capacity) return -1;
    *link_count = sink.count;
    return ghostty_render_state_clean(vt->render);
}

static uint32_t search_status(GhosttySearchStatus status) {
    switch (status) {
        case GHOSTTY_SEARCH_STATUS_RUNNING: return 0;
        case GHOSTTY_SEARCH_STATUS_FEED_REQUIRED: return 1;
        default: return 2;
    }
}

int silo_vt_search_set(SiloVt *vt, const unsigned char *needle, size_t len) {
    if (len == 0) {
        if (vt->search) {
            ghostty_search_free(vt->search);
            vt->search = NULL;
            /* The selected match was installed as the selection; drop it. */
            CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, NULL));
        }
        return 0;
    }
    if (!vt->search) CHECK(ghostty_search_new(NULL, &vt->search, vt->terminal));
    GhosttyString value = { .ptr = needle, .len = len };
    return ghostty_search_set(vt->search, GHOSTTY_SEARCH_OPT_NEEDLE, &value);
}

int silo_vt_search_step(SiloVt *vt, uint32_t max_ticks, uint32_t *status) {
    *status = 2;
    if (!vt->search) return 0;
    CHECK(ghostty_search_feed(vt->search));
    GhosttySearchStatus current = GHOSTTY_SEARCH_STATUS_COMPLETE;
    CHECK(ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_STATUS, &current));
    for (uint32_t i = 0; i < max_ticks && current == GHOSTTY_SEARCH_STATUS_RUNNING; ++i) {
        CHECK(ghostty_search_tick(vt->search, &current));
    }
    *status = search_status(current);
    return 0;
}

int silo_vt_search_select(SiloVt *vt, bool previous) {
    if (!vt->search) return 0;
    int result = ghostty_search_set(vt->search,
        previous ? GHOSTTY_SEARCH_OPT_SELECT_PREV : GHOSTTY_SEARCH_OPT_SELECT_NEXT, NULL);
    if (result == GHOSTTY_NO_VALUE) return 0;
    CHECK(result);
    GhosttySelection match = GHOSTTY_INIT_SIZED(GhosttySelection);
    result = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_SELECTED_MATCH, &match);
    if (result == GHOSTTY_NO_VALUE) return 0;
    CHECK(result);
    return ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, &match);
}

int silo_vt_search_state(SiloVt *vt, uint64_t *total, uint64_t *selected, uint32_t *status) {
    *total = 0;
    *selected = UINT64_MAX;
    *status = 2;
    if (!vt->search) return 0;
    size_t count = 0, index = 0;
    GhosttySearchStatus current = GHOSTTY_SEARCH_STATUS_COMPLETE;
    CHECK(ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_STATUS, &current));
    CHECK(ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_TOTAL_MATCHES, &count));
    int result = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_SELECTED_INDEX, &index);
    if (result == GHOSTTY_SUCCESS) *selected = index;
    else if (result != GHOSTTY_NO_VALUE) return result;
    *total = count;
    *status = search_status(current);
    return 0;
}

int silo_vt_search_viewport(SiloVt *vt, uint32_t *out, size_t cap, size_t *count) {
    *count = 0;
    if (!vt->search) return 0;
    GhosttySelection fixed[64];
    GhosttySelection *matches = fixed;
    GhosttySelectionBuffer buffer = { .ptr = fixed, .cap = 64, .len = 0 };
    int result = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES, &buffer);
    if (result == GHOSTTY_OUT_OF_SPACE) {
        /* Bounded retry with the required capacity; a page cannot hold more
         * matches than cells, and 4096 is well past any useful highlight count. */
        size_t needed = buffer.len < 4096 ? buffer.len : 4096;
        matches = calloc(needed, sizeof(*matches));
        if (!matches) return GHOSTTY_OUT_OF_MEMORY;
        buffer.ptr = matches;
        buffer.cap = needed;
        buffer.len = 0;
        result = ghostty_search_get(vt->search, GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES, &buffer);
        if (result == GHOSTTY_OUT_OF_SPACE) { buffer.len = 0; result = GHOSTTY_SUCCESS; }
    }
    if (result != GHOSTTY_SUCCESS) {
        if (matches != fixed) free(matches);
        return result;
    }
    for (size_t i = 0; i < buffer.len && *count < cap; ++i) {
        GhosttyPointCoordinate start, end;
        if (ghostty_terminal_point_from_grid_ref(vt->terminal, &matches[i].start,
                GHOSTTY_POINT_TAG_VIEWPORT, &start) != GHOSTTY_SUCCESS) continue;
        if (ghostty_terminal_point_from_grid_ref(vt->terminal, &matches[i].end,
                GHOSTTY_POINT_TAG_VIEWPORT, &end) != GHOSTTY_SUCCESS) continue;
        uint32_t *span = out + 4 * (*count)++;
        span[0] = start.x;
        span[1] = start.y;
        span[2] = end.x;
        span[3] = end.y;
    }
    if (matches != fixed) free(matches);
    return 0;
}

static GhosttyKey key_code(const char *name) {
    if (name[0] && !name[1]) {
        if (*name >= 'a' && *name <= 'z') return GHOSTTY_KEY_A + (*name - 'a');
        if (*name >= '0' && *name <= '9') return GHOSTTY_KEY_DIGIT_0 + (*name - '0');
    }
    static const struct { const char *name; GhosttyKey key; } keys[] = {
        {"enter", GHOSTTY_KEY_ENTER}, {"backspace", GHOSTTY_KEY_BACKSPACE},
        {"tab", GHOSTTY_KEY_TAB}, {"escape", GHOSTTY_KEY_ESCAPE},
        {"space", GHOSTTY_KEY_SPACE}, {"up", GHOSTTY_KEY_ARROW_UP},
        {"down", GHOSTTY_KEY_ARROW_DOWN}, {"left", GHOSTTY_KEY_ARROW_LEFT},
        {"right", GHOSTTY_KEY_ARROW_RIGHT}, {"home", GHOSTTY_KEY_HOME},
        {"end", GHOSTTY_KEY_END}, {"pageup", GHOSTTY_KEY_PAGE_UP},
        {"pagedown", GHOSTTY_KEY_PAGE_DOWN}, {"delete", GHOSTTY_KEY_DELETE},
        {"insert", GHOSTTY_KEY_INSERT}, {"-", GHOSTTY_KEY_MINUS},
        {"=", GHOSTTY_KEY_EQUAL}, {"[", GHOSTTY_KEY_BRACKET_LEFT},
        {"]", GHOSTTY_KEY_BRACKET_RIGHT}, {"\\", GHOSTTY_KEY_BACKSLASH},
        {";", GHOSTTY_KEY_SEMICOLON}, {"'", GHOSTTY_KEY_QUOTE},
        {",", GHOSTTY_KEY_COMMA}, {".", GHOSTTY_KEY_PERIOD},
        {"/", GHOSTTY_KEY_SLASH}, {"`", GHOSTTY_KEY_BACKQUOTE},
        {"f1", GHOSTTY_KEY_F1}, {"f2", GHOSTTY_KEY_F2}, {"f3", GHOSTTY_KEY_F3},
        {"f4", GHOSTTY_KEY_F4}, {"f5", GHOSTTY_KEY_F5}, {"f6", GHOSTTY_KEY_F6},
        {"f7", GHOSTTY_KEY_F7}, {"f8", GHOSTTY_KEY_F8}, {"f9", GHOSTTY_KEY_F9},
        {"f10", GHOSTTY_KEY_F10}, {"f11", GHOSTTY_KEY_F11}, {"f12", GHOSTTY_KEY_F12},
    };
    for (size_t i = 0; i < sizeof(keys)/sizeof(keys[0]); ++i)
        if (!strcmp(name, keys[i].name)) return keys[i].key;
    return GHOSTTY_KEY_UNIDENTIFIED;
}

int silo_vt_key(SiloVt *vt, const char *key, const char *text, size_t len,
                    uint32_t mods, bool release, char *out, size_t cap, size_t *written) {
    GhosttyKeyEvent event = NULL;
    CHECK(ghostty_key_event_new(NULL, &event));
    ghostty_key_encoder_setopt_from_terminal(vt->encoder, vt->terminal);
    ghostty_key_event_set_key(event, key_code(key));
    ghostty_key_event_set_action(event, release ? GHOSTTY_KEY_ACTION_RELEASE : GHOSTTY_KEY_ACTION_PRESS);
    GhosttyMods m = 0;
    if (mods & 1) m |= GHOSTTY_MODS_SHIFT;
    if (mods & 2) m |= GHOSTTY_MODS_CTRL;
    if (mods & 4) m |= GHOSTTY_MODS_ALT;
    if (mods & 8) m |= GHOSTTY_MODS_SUPER;
    ghostty_key_event_set_mods(event, m);
    ghostty_key_event_set_utf8(event, text, len);
    if (key[0] && !key[1]) ghostty_key_event_set_unshifted_codepoint(event, (unsigned char)key[0]);
    int result = ghostty_key_encoder_encode(vt->encoder, event, out, cap, written);
    ghostty_key_event_free(event);
    return result;
}

static bool contains(const char *data, size_t len, const char *needle, size_t n) {
    for (size_t i = 0; i + n <= len; i++)
        if (memcmp(data + i, needle, n) == 0) return true;
    return false;
}

int silo_vt_paste(SiloVt *vt, char *data, size_t len, bool line_breaks, char *out, size_t cap, size_t *written) {
    GhosttyTerminalModeConfig mode = { .mode = GHOSTTY_MODE_BRACKETED_PASTE };
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_MODE, &mode));
    /* Upstream's terminal-aware rule (ghostty_terminal_paste): line breaks
     * run commands only outside a bracketed paste (-2 unless the user
     * allowed them); inside one only its terminator can (-3). */
    static const char end[] = "\x1b[201~";
    if (mode.value && contains(data, len, end, sizeof end - 1)) return -3;
    if (!mode.value && !line_breaks && (contains(data, len, "\n", 1) || contains(data, len, "\r", 1))) return -2;
    return ghostty_paste_encode(data, len, mode.value, out, cap, written);
}

static int select_pointer(SiloVt *vt, const uint32_t *f, uint64_t ns) {
    if (f[1] != 1) return 0;
    GhosttyPoint at = { .tag = GHOSTTY_POINT_TAG_VIEWPORT,
        .value.coordinate = { .x = f[2] / f[5], .y = f[3] / f[6] } };
    /* Shift-press extends an existing selection to the pointer; further
     * drags keep moving its end until release. */
    if ((f[0] == 0 && (f[4] & 1)) || (vt->extending && f[0] != 0)) {
        GhosttySelection current = GHOSTTY_INIT_SIZED(GhosttySelection);
        int got = ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_SELECTION, &current);
        if (got == GHOSTTY_SUCCESS) {
            if (f[0] == 1) {
                vt->extending = false;
                vt->selecting = false;
                return 0;
            }
            GhosttyGridRef end = GHOSTTY_INIT_SIZED(GhosttyGridRef);
            CHECK(ghostty_terminal_grid_ref(vt->terminal, at, &end));
            current.end = end;
            vt->extending = true;
            vt->selecting = true;
            return ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, &current);
        }
        vt->extending = false;
    }
    GhosttySelectionGestureEventType type = f[0] == 0 ? GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS :
        (f[0] == 1 ? GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE : GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG);
    if (f[0] == 0) {
        CHECK(ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, NULL));
        vt->selecting = true;
    } else if (!vt->selecting) return 0;
    GhosttyGridRef ref = GHOSTTY_INIT_SIZED(GhosttyGridRef);
    CHECK(ghostty_terminal_grid_ref(vt->terminal, at, &ref));
    GhosttySelectionGestureEvent event = NULL;
    CHECK(ghostty_selection_gesture_event_new(NULL, &event, type));
    GhosttySurfacePosition pos = { .x = f[2], .y = f[3] };
    GhosttySelectionGestureGeometry geometry = { .columns = f[7] / f[5], .cell_width = f[5], .screen_height = f[8] };
    int result = ghostty_selection_gesture_event_set(event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF, &ref);
    if (f[0] == 0) {
        {
            uint64_t interval = 500000000;
            double distance = (double)f[5];
            if (!result) result = ghostty_selection_gesture_event_set(event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS, &ns);
            if (!result) result = ghostty_selection_gesture_event_set(event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS, &interval);
            if (!result) result = ghostty_selection_gesture_event_set(event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE, &distance);
        }
    }
    if (!result && f[0] != 1) result = ghostty_selection_gesture_event_set(event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION, &pos);
    if (!result && f[0] == 2) result = ghostty_selection_gesture_event_set(event, GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY, &geometry);
    GhosttySelection selection = GHOSTTY_INIT_SIZED(GhosttySelection);
    if (!result) result = ghostty_selection_gesture_event(vt->selection, vt->terminal, event, &selection);
    ghostty_selection_gesture_event_free(event);
    if (f[0] == 1) vt->selecting = false;
    if (result == GHOSTTY_NO_VALUE) return 0;
    if (result) return result;
    return ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, &selection);
}

bool silo_vt_drag_scroll(SiloVt *vt, int32_t lines) {
    if (!vt->selecting) return false;
    GhosttyTerminalScrollViewport scroll = { .tag = GHOSTTY_SCROLL_VIEWPORT_DELTA, .value.delta = lines };
    ghostty_terminal_scroll_viewport(vt->terminal, scroll);
    return true;
}

void silo_vt_cancel_drag(SiloVt *vt) {
    vt->selecting = false;
    vt->extending = false;
    ghostty_selection_gesture_reset(vt->selection, vt->terminal);
}

bool silo_vt_clear_selection(SiloVt *vt) {
    GhosttySelection current = GHOSTTY_INIT_SIZED(GhosttySelection);
    if (ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_SELECTION, &current) != GHOSTTY_SUCCESS) return false;
    vt->selecting = false;
    vt->extending = false;
    ghostty_selection_gesture_reset(vt->selection, vt->terminal);
    ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, NULL);
    return true;
}

bool silo_vt_select_all(SiloVt *vt) {
    GhosttySelection all = GHOSTTY_INIT_SIZED(GhosttySelection);
    if (ghostty_terminal_select_all(vt->terminal, &all) != GHOSTTY_SUCCESS) return false;
    vt->selecting = false;
    vt->extending = false;
    return ghostty_terminal_set(vt->terminal, GHOSTTY_TERMINAL_OPT_SELECTION, &all) == GHOSTTY_SUCCESS;
}

int silo_vt_copy(SiloVt *vt, unsigned char *out, size_t cap, size_t *written) {
    GhosttyTerminalSelectionFormatOptions options = GHOSTTY_INIT_SIZED(GhosttyTerminalSelectionFormatOptions);
    options.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN;
    options.unwrap = true;
    options.trim = true;
    *written = 0;
    int result = ghostty_terminal_selection_format_buf(vt->terminal, options, out, cap, written);
    return result == GHOSTTY_NO_VALUE ? 0 : result;
}

int silo_vt_mouse(SiloVt *vt, const uint32_t *fields, uint64_t timestamp, char *out, size_t cap, size_t *written) {
    static const GhosttyMouseAction actions[] = {GHOSTTY_MOUSE_ACTION_PRESS, GHOSTTY_MOUSE_ACTION_RELEASE, GHOSTTY_MOUSE_ACTION_MOTION};
    static const GhosttyMouseButton buttons[] = {GHOSTTY_MOUSE_BUTTON_UNKNOWN, GHOSTTY_MOUSE_BUTTON_LEFT,
        GHOSTTY_MOUSE_BUTTON_RIGHT, GHOSTTY_MOUSE_BUTTON_MIDDLE, GHOSTTY_MOUSE_BUTTON_FOUR, GHOSTTY_MOUSE_BUTTON_FIVE};
    if (fields[0] >= 3 || fields[1] >= 6) return -1;
    GhosttyMouseEncoderSize size = GHOSTTY_INIT_SIZED(GhosttyMouseEncoderSize);
    size.cell_width = fields[5]; size.cell_height = fields[6];
    size.screen_width = fields[7]; size.screen_height = fields[8];
    if (!size.cell_width || !size.cell_height) return -1;
    bool tracking = false;
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, &tracking));
    if (!tracking || (fields[4] & 1) || vt->selecting) {
        *written = 0;
        return select_pointer(vt, fields, timestamp);
    }
    ghostty_mouse_encoder_setopt_from_terminal(vt->mouse, vt->terminal);
    ghostty_mouse_encoder_setopt(vt->mouse, GHOSTTY_MOUSE_ENCODER_OPT_SIZE, &size);
    bool pressed = fields[1] >= 1 && fields[1] <= 3 && fields[0] != 1;
    ghostty_mouse_encoder_setopt(vt->mouse, GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED, &pressed);
    GhosttyMouseEvent event = NULL;
    CHECK(ghostty_mouse_event_new(NULL, &event));
    ghostty_mouse_event_set_action(event, actions[fields[0]]);
    ghostty_mouse_event_set_button(event, buttons[fields[1]]);
    ghostty_mouse_event_set_position(event, (GhosttyMousePosition){ .x = fields[2], .y = fields[3] });
    GhosttyMods mods = 0;
    if (fields[4] & 1) mods |= GHOSTTY_MODS_SHIFT;
    if (fields[4] & 2) mods |= GHOSTTY_MODS_CTRL;
    if (fields[4] & 4) mods |= GHOSTTY_MODS_ALT;
    ghostty_mouse_event_set_mods(event, mods);
    int result = ghostty_mouse_encoder_encode(vt->mouse, event, out, cap, written);
    ghostty_mouse_event_free(event);
    return result;
}

int silo_vt_focus(SiloVt *vt, bool focused, char *out, size_t cap, size_t *written) {
    GhosttyTerminalModeConfig mode = { .mode = GHOSTTY_MODE_FOCUS_EVENT };
    CHECK(ghostty_terminal_get(vt->terminal, GHOSTTY_TERMINAL_DATA_MODE, &mode));
    *written = 0;
    if (!mode.value) return GHOSTTY_SUCCESS;
    return ghostty_focus_encode(focused ? GHOSTTY_FOCUS_GAINED : GHOSTTY_FOCUS_LOST,
                                out, cap, written);
}

void silo_vt_scroll(SiloVt *vt, int32_t delta) {
    GhosttyTerminalScrollViewport scroll = { .tag = GHOSTTY_SCROLL_VIEWPORT_DELTA, .value.delta = delta };
    ghostty_terminal_scroll_viewport(vt->terminal, scroll);
}

void silo_vt_scroll_to_row(SiloVt *vt, uint64_t row) {
    GhosttyTerminalScrollViewport scroll = { .tag = GHOSTTY_SCROLL_VIEWPORT_ROW, .value.row = (size_t)row };
    ghostty_terminal_scroll_viewport(vt->terminal, scroll);
}

void silo_vt_scroll_prompt(SiloVt *vt, int32_t delta) {
    GhosttyTerminalScrollViewport scroll = { .tag = GHOSTTY_SCROLL_VIEWPORT_DELTA_PROMPT, .value.delta_prompt = delta };
    ghostty_terminal_scroll_viewport(vt->terminal, scroll);
}
