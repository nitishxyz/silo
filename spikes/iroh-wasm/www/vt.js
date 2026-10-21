// Decode a GHOSTSNP snapshot with libghostty-vt's own wasm build
// (`zig build -Demit-lib-vt=true -Dtarget=wasm32-freestanding`, copied to
// www/vt/ghostty-vt.wasm by build-vt.sh) and print the screen as plain text.
// Works in the browser and in node (pass `wasmBytes`).

export async function decode(snapshot, log, wasmBytes) {
  const t0 = performance.now();
  const source = wasmBytes ?? (await fetch(new URL("./vt/ghostty-vt.wasm", import.meta.url)).then((r) => r.arrayBuffer()));
  const { instance } = await WebAssembly.instantiate(source, {});
  const e = instance.exports;
  const tInit = performance.now() - t0;
  const bytes = () => new Uint8Array(e.memory.buffer);
  const view = () => new DataView(e.memory.buffer);
  const check = (what, r) => {
    if (r !== 0) throw new Error(`${what}: libghostty-vt error ${r}`);
  };
  const slot = e.ghostty_wasm_alloc_opaque();

  const t1 = performance.now();
  const buf = e.ghostty_wasm_alloc(snapshot.length);
  bytes().set(snapshot, buf);
  check("decoder_new_buf", e.ghostty_snapshot_decoder_new_buf(0, slot, buf, snapshot.length));
  const decoder = e.ghostty_wasm_take_opaque(slot);
  check("decoder_decode", e.ghostty_snapshot_decoder_decode(decoder, slot));
  const terminal = e.ghostty_wasm_take_opaque(slot);
  e.ghostty_snapshot_decoder_free(decoder);
  e.ghostty_wasm_free(buf, snapshot.length);
  const tDecode = performance.now() - t1;

  // GhosttyFormatterTerminalOptions (40 bytes on wasm32, see ghostty_type_json),
  // passed by pointer per the wasm32 C ABI for aggregates.
  const opts = e.ghostty_wasm_alloc(40);
  bytes().fill(0, opts, opts + 40);
  const dv = view();
  dv.setUint32(opts + 0, 40, true); // size
  dv.setInt32(opts + 4, 0, true); // emit = PLAIN
  dv.setUint8(opts + 8, 1); // unwrap
  dv.setUint8(opts + 9, 1); // trim
  dv.setUint32(opts + 12, 24, true); // extra.size
  dv.setUint32(opts + 24, 12, true); // extra.screen.size
  check("formatter_terminal_new", e.ghostty_formatter_terminal_new(0, slot, terminal, opts));
  const formatter = e.ghostty_wasm_take_opaque(slot);
  const out = e.ghostty_wasm_alloc(8);
  check("formatter_format_alloc", e.ghostty_formatter_format_alloc(formatter, 0, out, out + 4));
  const ptr = view().getUint32(out, true);
  const len = view().getUint32(out + 4, true);
  const text = new TextDecoder().decode(bytes().slice(ptr, ptr + len));
  e.ghostty_free(0, ptr, len);
  e.ghostty_wasm_free(out, 8);
  e.ghostty_wasm_free(opts, 40);
  e.ghostty_formatter_free(formatter);
  e.ghostty_terminal_free(terminal);
  e.ghostty_wasm_free_opaque(slot);

  log(`ghostty decode ok: instantiate ${tInit.toFixed(0)} ms, decode ${tDecode.toFixed(1)} ms, ${len} B text`);
  log("screen:\n" + text);
  return { ok: true, instantiate_ms: tInit, decode_ms: tDecode, text, wasm_bytes: source.byteLength };
}
