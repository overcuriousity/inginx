// Minimal WebAuthn helpers: base64url <-> ArrayBuffer plumbing around
// navigator.credentials for the webauthn-rs JSON challenge format.
(function () {
  function b64uToBuf(s) {
    s = s.replace(/-/g, "+").replace(/_/g, "/");
    while (s.length % 4) s += "=";
    const bin = atob(s);
    const buf = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i);
    return buf.buffer;
  }
  function bufToB64u(buf) {
    const bytes = new Uint8Array(buf);
    let bin = "";
    for (const b of bytes) bin += String.fromCharCode(b);
    return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  async function post(url, body) {
    const r = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!r.ok) throw new Error(url + " failed: " + r.status + " " + (await r.text()));
    return r.json();
  }

  window.inginxRegisterPasskey = async function () {
    const ccr = await post("/admin/webauthn/register/start");
    const pk = ccr.publicKey;
    pk.challenge = b64uToBuf(pk.challenge);
    pk.user.id = b64uToBuf(pk.user.id);
    (pk.excludeCredentials || []).forEach((c) => (c.id = b64uToBuf(c.id)));
    const cred = await navigator.credentials.create({ publicKey: pk });
    await post("/admin/webauthn/register/finish", {
      id: cred.id,
      rawId: bufToB64u(cred.rawId),
      type: cred.type,
      extensions: cred.getClientExtensionResults(),
      response: {
        attestationObject: bufToB64u(cred.response.attestationObject),
        clientDataJSON: bufToB64u(cred.response.clientDataJSON),
      },
    });
  };

  window.inginxAuthenticatePasskey = async function () {
    const rcr = await post("/admin/webauthn/auth/start");
    const pk = rcr.publicKey;
    pk.challenge = b64uToBuf(pk.challenge);
    (pk.allowCredentials || []).forEach((c) => (c.id = b64uToBuf(c.id)));
    const cred = await navigator.credentials.get({ publicKey: pk });
    await post("/admin/webauthn/auth/finish", {
      id: cred.id,
      rawId: bufToB64u(cred.rawId),
      type: cred.type,
      extensions: cred.getClientExtensionResults(),
      response: {
        authenticatorData: bufToB64u(cred.response.authenticatorData),
        clientDataJSON: bufToB64u(cred.response.clientDataJSON),
        signature: bufToB64u(cred.response.signature),
        userHandle: cred.response.userHandle ? bufToB64u(cred.response.userHandle) : null,
      },
    });
  };

  // The buttons wire themselves from data attributes rather than an inline
  // handler, so the 2FA and settings pages need no `script-src` exception. A
  // failure is written to `[data-passkey-error]`; enrol reloads, auth follows
  // `data-passkey-redirect`.
  function wire() {
    var err = document.querySelector("[data-passkey-error]");
    var fail = function (e) { if (err) err.textContent = e.message || String(e); };

    var enroll = document.querySelector("[data-passkey-enroll]");
    if (enroll) {
      enroll.addEventListener("click", async function () {
        try { await window.inginxRegisterPasskey(); location.reload(); }
        catch (e) { fail(e); }
      });
    }
    var auth = document.querySelector("[data-passkey-auth]");
    if (auth) {
      auth.addEventListener("click", async function () {
        try {
          await window.inginxAuthenticatePasskey();
          location.href = auth.getAttribute("data-passkey-redirect") || "/admin";
        } catch (e) { fail(e); }
      });
    }
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", wire);
  } else {
    wire();
  }
})();
