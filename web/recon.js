/* OSTARA recon.js — RECON toolkit drawer.
 * Every endpoint below is a real backend route (see README API reference).
 * Port scanning is gated server-side by RECON_ENABLED=true; the
 * responsible-use notice is shown inline, always. */

window.OST = window.OST || {};
(function (OST) {
  "use strict";
  const esc = OST.esc;

  const TOOLS = [
    {
      id: "dns", label: "DNS", endpoint: (v) => `/api/recon/dns?domain=${encodeURIComponent(v)}`,
      inputs: [{ name: "domain", placeholder: "domain, e.g. example.com" }],
      hint: "A / AAAA / MX / TXT / NS via explicit resolvers (no system stub).",
    },
    {
      id: "whois", label: "WHOIS", endpoint: (v) => `/api/recon/whois?domain=${encodeURIComponent(v)}`,
      inputs: [{ name: "domain", placeholder: "domain, e.g. example.com" }],
      hint: "WHOIS with IANA referral following.",
    },
    {
      id: "ct", label: "Cert transparency", endpoint: (v) => `/api/recon/ct?domain=${encodeURIComponent(v)}`,
      inputs: [{ name: "domain", placeholder: "domain, e.g. example.com" }],
      hint: "crt.sh certificate transparency log search.",
    },
    {
      id: "ip", label: "IP / ASN", endpoint: (v) => `/api/recon/ip?ip=${encodeURIComponent(v)}`,
      inputs: [{ name: "ip", placeholder: "IP address, e.g. 8.8.8.8" }],
      hint: "IP and ASN enrichment (ip-api.com → ipinfo.io).",
    },
    {
      id: "cve", label: "CVE", endpoint: (v) => `/api/recon/cve?id=${encodeURIComponent(v)}`,
      inputs: [{ name: "id", placeholder: "CVE id, e.g. CVE-2024-1234" }],
      hint: "CVE lookup via NVD API v2.",
    },
    {
      id: "scan", label: "Port scan", post: true,
      inputs: [
        { name: "target", placeholder: "target host or IP (you own / are authorized)" },
        { name: "ports", placeholder: "ports, e.g. 22,80,443,8080", value: "22,80,443" },
      ],
      hint: "TCP connect scan. Gated server-side by RECON_ENABLED=true (default off → 503).",
      warn: "Only scan infrastructure you own or are authorized to test.",
    },
  ];

  function kvTable(obj, depth) {
    if (depth > 3) return `<span class="m">${esc(JSON.stringify(obj))}</span>`;
    if (obj == null) return `<span class="m">—</span>`;
    if (Array.isArray(obj)) {
      if (!obj.length) return `<span class="m">(empty)</span>`;
      if (obj.every((x) => typeof x !== "object" || x === null))
        return obj.map((x) => `<div>${esc(String(x))}</div>`).join("");
      return obj.slice(0, 50).map((x) => `<div style="margin-bottom:6px">${kvTable(x, depth + 1)}</div>`).join("") +
        (obj.length > 50 ? `<div class="m">… ${obj.length - 50} more</div>` : "");
    }
    if (typeof obj !== "object") return esc(String(obj));
    const rows = Object.entries(obj)
      .filter(([k]) => !/generated_at/i.test(k))
      .map(([k, v]) => `<tr><td>${esc(k)}</td><td>${kvTable(v, depth + 1)}</td></tr>`)
      .join("");
    return `<table class="kv">${rows}</table>`;
  }

  let active = TOOLS[0].id;

  function renderTabs() {
    document.getElementById("recon-tabs").innerHTML = TOOLS.map((t) =>
      `<button data-rtool="${t.id}" class="${t.id === active ? "on" : ""}">${esc(t.label)}</button>`).join("");
  }

  function renderBody() {
    const t = TOOLS.find((x) => x.id === active);
    const body = document.getElementById("recon-body");
    body.innerHTML =
      (t.warn ? `<div class="warnbox">⚠ ${esc(t.warn)}</div>` : "") +
      `<div class="recon-form">` +
      t.inputs.map((i) =>
        `<input data-rin="${i.name}" placeholder="${esc(i.placeholder)}" value="${esc(i.value || "")}" spellcheck="false" />`).join("") +
      `<button id="recon-go">Run</button></div>` +
      `<div class="m" style="margin:6px 0">${esc(t.hint)}</div>` +
      `<div id="recon-out"></div>`;
    const go = () => runTool(t);
    document.getElementById("recon-go").onclick = go;
    body.querySelectorAll("input").forEach((inp) =>
      inp.addEventListener("keydown", (e) => { if (e.key === "Enter") go(); }));
  }

  async function runTool(t) {
    const out = document.getElementById("recon-out");
    const vals = {};
    document.querySelectorAll("#recon-body [data-rin]").forEach((i) => { vals[i.dataset.rin] = i.value.trim(); });
    const first = t.inputs[0].name;
    if (!vals[first]) { out.innerHTML = `<div class="errbox">enter ${esc(first)}</div>`; return; }
    out.innerHTML = `<div class="m">running…</div>`;
    try {
      let res;
      if (t.post) {
        const ports = vals.ports.split(/[,\s]+/).map((s) => parseInt(s, 10)).filter((n) => n > 0 && n < 65536);
        if (!ports.length) { out.innerHTML = `<div class="errbox">enter at least one valid port</div>`; return; }
        res = await OST.fetchJSON("/api/recon/scan", {
          timeout: 60000,
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ target: vals.target, ports, timeout_ms: 2000 }),
        });
      } else {
        res = await OST.fetchJSON(t.endpoint(vals[first]), { timeout: 30000 });
      }
      out.innerHTML = kvTable(res.data, 0);
    } catch (e) { OST.showFetchError(out, e, t.label); }
  }

  OST.initRecon = function () {
    renderTabs();
    renderBody();
    document.getElementById("recon-tabs").addEventListener("click", (e) => {
      const id = e.target.dataset && e.target.dataset.rtool;
      if (!id) return;
      active = id; renderTabs(); renderBody();
    });
  };
})(window.OST);
