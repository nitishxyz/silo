import init, { run, share_ticket_from_pair } from "./pkg/silo_iroh_wasm_spike.js";

const out = document.getElementById("log");
const lines = [];
const t0 = performance.now();
function log(s) {
  const line = `[${(performance.now() - t0).toFixed(0).padStart(6)} ms] ${s}`;
  lines.push(line);
  out.textContent += line + "\n";
}

async function report(result) {
  result.log = lines;
  result.userAgent = navigator.userAgent;
  // The static server (serve.py) writes this to result.json.
  await fetch("/result", { method: "POST", body: JSON.stringify(result, null, 2) }).catch(() => {});
  document.title = result.ok ? "spike: ok" : "spike: failed";
}

async function main() {
  // The ticket never leaves the page: fragments aren't sent to the server.
  const ticket = decodeURIComponent(location.hash.slice(1));
  const params = new URLSearchParams(location.search);
  const vt = params.get("vt") || "";
  const waitOnline = params.get("online") !== "0";
  const relayOnly = params.get("all_addrs") !== "1";
  if (!ticket) throw new Error("no ticket in location.hash");
  const tInit = performance.now();
  await init();
  log(`wasm instantiated in ${(performance.now() - tInit).toFixed(0)} ms`);

  const share = share_ticket_from_pair(ticket, vt);
  log(`silos1 ShareTicket round trip ok: ${share.length} chars, link ${location.origin}/#${share}`.slice(0, 200));

  const tRun = performance.now();
  const { report: r, snapshot } = await run(ticket, vt, waitOnline, relayOnly, log);
  r.wait_online = waitOnline;
  r.relay_only = relayOnly;
  r.run_ms_js = performance.now() - tRun;
  r.share_ticket_len = share.length;
  r.share_link_len = `https://silo.example/#${share}`.length;
  log(`snapshot bytes in JS: ${snapshot.length}`);

  let decode = null;
  try {
    const vtmod = await import("./vt.js");
    decode = await vtmod.decode(snapshot, log);
  } catch (e) {
    decode = { ok: false, error: String(e) };
    log(`ghostty decode: ${e}`);
  }
  await report({ ok: true, report: r, decode });
}

main().catch(async (e) => {
  log(`FAILED: ${e}`);
  await report({ ok: false, error: String(e) });
});
