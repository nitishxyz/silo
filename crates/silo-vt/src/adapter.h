#ifndef SILO_VT_ADAPTER_H
#define SILO_VT_ADAPTER_H
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct SiloVt SiloVt;
typedef struct {
    unsigned char text[256];
    uint32_t len, foreground, background, flags, width;
} SiloCell;
typedef struct {
    uint64_t scroll_total, scroll_offset, scroll_len;
    uint32_t background, cursor_x, cursor_y, cursor_visible;
    uint32_t cursor_shape, cursor_blink, cursor_color, mouse_tracking;
    /* 0 clean, 1 some rows (see dirty_rows), 2 every row was extracted. */
    uint32_t dirty;
    /* Cursor sits on an OSC 133 prompt/input row (shell integration). */
    uint32_t at_prompt;
    /* The alternate screen is active. */
    uint32_t alt_screen;
    /* Synchronized output (mode 2026) is set: the program is mid-update. */
    uint32_t synchronized;
} SiloFrame;
/* One OSC 8 hyperlink span on an extracted viewport row; `end` is exclusive. */
typedef struct {
    uint32_t row, start, end, len;
    unsigned char uri[1024];
} SiloLink;
/* Effects accumulated since the previous silo_vt_events call. */
typedef struct {
    /* reserved: bit 0 standard clipboard, bit 1 primary selection. */
    uint32_t bells, title_changed, pwd_changed, reserved;
} SiloEvents;

SiloVt *silo_vt_new(uint16_t columns, uint16_t rows, size_t continuation_max);
void silo_vt_free(SiloVt *vt);
/* columns, rows, width_px, height_px */
int silo_vt_size(SiloVt *vt, uint32_t *out);
int silo_vt_ground(SiloVt *vt, bool *ground);
void silo_vt_reset(SiloVt *vt);
int silo_vt_snapshot(SiloVt *vt, uint8_t **out, size_t *len);
void silo_bytes_free(uint8_t *ptr, size_t len);
SiloVt *silo_vt_restore(const uint8_t *head, size_t len, bool retain_continuation,
                        bool compress_history, uint64_t *history_rows, int *error);
int silo_vt_restore_push(SiloVt *vt, const uint8_t *data, size_t len);
int silo_vt_restore_next(SiloVt *vt, size_t *rows);
bool silo_vt_drag_scroll(SiloVt *vt, int32_t lines);
void silo_vt_cancel_drag(SiloVt *vt);
/* Clear the selection; returns whether one existed. */
bool silo_vt_clear_selection(SiloVt *vt);
/* Select all selectable content; returns whether anything was selected. */
bool silo_vt_select_all(SiloVt *vt);
int silo_vt_copy(SiloVt *vt, unsigned char *out, size_t cap, size_t *written);
int silo_vt_write(SiloVt *vt, const unsigned char *data, size_t len);
size_t silo_vt_replies(SiloVt *vt, unsigned char *out);
int silo_vt_resize(SiloVt *vt, uint16_t columns, uint16_t rows, uint16_t width, uint16_t height);
/* Extracts every row when `full` or upstream reports a full redraw; otherwise
 * only rows upstream marks dirty, whose bits are set in dirty_rows[4]. Rows
 * that were not extracted leave their cells untouched. Hyperlink spans are
 * reported only for extracted rows, at most `link_cap` per call. */
int silo_vt_frame(SiloVt *vt, SiloFrame *out, SiloCell *cells, size_t capacity,
                      uint16_t columns, bool full, uint64_t *dirty_rows,
                      SiloLink *links, size_t link_cap, size_t *link_count);
/* Returns and clears accumulated bell/title/pwd effects. */
void silo_vt_events(SiloVt *vt, SiloEvents *out);
/* Copies and clears the last program clipboard write; returns required length if cap is short. */
size_t silo_vt_clipboard(SiloVt *vt, int slot, unsigned char *out, size_t cap);
/* Copies the current title (which == 0) or pwd (which == 1) bytes, truncated
 * to cap; `written` is the copied length. */
int silo_vt_string(SiloVt *vt, int which, unsigned char *out, size_t cap, size_t *written);
/* Search. An empty needle clears the search and releases its state. */
int silo_vt_search_set(SiloVt *vt, const unsigned char *needle, size_t len);
/* One feed plus up to max_ticks ticks. Writes status: 0 running, 1 feed
 * required, 2 complete. No-op (status 2) without an active search. */
int silo_vt_search_step(SiloVt *vt, uint32_t max_ticks, uint32_t *status);
/* Selects the next (older) or previous (newer) match, scrolls it into view if
 * needed, and installs it as the terminal selection. No matches is success. */
int silo_vt_search_select(SiloVt *vt, bool previous);
/* total matches, selected index (UINT64_MAX when none) and status as above. */
int silo_vt_search_state(SiloVt *vt, uint64_t *total, uint64_t *selected, uint32_t *status);
/* Viewport matches as inclusive viewport coordinates, four values per span:
 * start_x, start_y, end_x, end_y. Matches outside the viewport are skipped. */
int silo_vt_search_viewport(SiloVt *vt, uint32_t *out, size_t cap, size_t *count);
int silo_vt_key(SiloVt *vt, const char *key, const char *text, size_t len,
                    uint32_t mods, bool release, char *out, size_t cap, size_t *written);
int silo_vt_paste(SiloVt *vt, char *data, size_t len, bool line_breaks, char *out, size_t cap, size_t *written);
void silo_vt_scroll(SiloVt *vt, int32_t delta);
void silo_vt_scroll_to_row(SiloVt *vt, uint64_t row);
int silo_vt_scrollbar(SiloVt *vt, uint64_t *out);
int silo_vt_focus(SiloVt *vt, bool focused, char *out, size_t cap, size_t *written);
int silo_vt_mouse(SiloVt *vt, const uint32_t *fields, uint64_t timestamp, char *out, size_t cap, size_t *written);
/* colors: foreground, background, cursor, then ANSI 0-15, each 0x00RRGGBB.
 * `dark` is the scheme reported to CSI ? 996 n; when it changes and the
 * program enabled mode 2031, the report to write to the PTY is placed in out. */
int silo_vt_set_colors(SiloVt *vt, const uint32_t *colors, bool dark,
                           char *out, size_t cap, size_t *written);
/* User defaults from terminal.conf. `cursor_style` is a
 * GhosttyTerminalCursorStyle value (bar 0, block 1, underline 2, hollow 3),
 * or -1 to restore upstream's built-in default; `cursor_blink` is 0, 1, or -1
 * likewise. Both are the defaults DECSCUSR reset restores, so a program's own
 * choice still wins. The scrollback bounds are applied immediately. */
int silo_vt_set_defaults(SiloVt *vt, int32_t cursor_style, int32_t cursor_blink,
                             uint64_t scrollback_bytes, uint64_t scrollback_lines);
#endif
