//! The page-side halves of the actions, run with Runtime.evaluate (returnByValue). Each is a function expression
//! called with JSON arguments, so no caller text is ever spliced into code.

/// (max_chars, links) -> {title, url, length, text, linkCount?, links?: [[text, href]]}
pub const READ: &str = r#"((max, links) => {
  const t = document.body ? document.body.innerText : '';
  const out = {title: document.title, url: location.href, length: t.length, text: t.slice(0, max)};
  if (links) {
    const a = [...document.querySelectorAll('a[href]')];
    out.linkCount = a.length;
    out.links = a.slice(0, 200).map(x => [(x.innerText || x.title || x.getAttribute('aria-label') || '').replace(/\s+/g, ' ').trim().slice(0, 120), x.href]);
  }
  return out;
})"#;

/// (text|null, href|null, index) -> {count, href?, text?}
pub const FIND_LINK: &str = r#"((text, href, index) => {
  const norm = s => (s || '').replace(/\s+/g, ' ').trim().toLowerCase();
  const m = [...document.querySelectorAll('a[href]')].filter(a =>
    (text === null || norm(a.innerText || a.title || a.getAttribute('aria-label')).includes(norm(text))) &&
    (href === null || a.href === href || a.getAttribute('href') === href || a.href.includes(href)));
  if (index >= m.length) return {count: m.length};
  return {count: m.length, href: m[index].href, text: norm(m[index].innerText).slice(0, 200)};
})"#;

/// ([[key, value]], submit) -> {set: [key], submitted, url} | {error}. All fields are found and checked before
/// any is set. Password, card and file fields are refused: Georg types those himself.
pub const FILL: &str = r#"((fields, submit) => {
  const find = k => {
    let el = null;
    try { el = document.querySelector('[name="' + CSS.escape(k) + '"]'); } catch (e) {}
    if (!el) el = document.getElementById(k);
    if (!el) { try { el = document.querySelector(k); } catch (e) {} }
    return el;
  };
  const plan = [];
  for (const [k, v] of fields) {
    const el = find(k);
    if (!el) return {error: 'no field ' + JSON.stringify(k) + ' (by name, id or CSS selector)'};
    const tag = el.tagName.toLowerCase(), type = (el.type || '').toLowerCase();
    if (!['input', 'textarea', 'select'].includes(tag)) return {error: JSON.stringify(k) + ' is a ' + tag + ', not a form field'};
    if (type === 'password' || type === 'file' || /^cc-/.test(el.autocomplete || ''))
      return {error: JSON.stringify(k) + ': password, card and file fields are refused (Georg types those himself)'};
    if (el.disabled || el.readOnly) return {error: JSON.stringify(k) + ' is disabled or read-only'};
    if (['submit', 'button', 'reset', 'image', 'hidden'].includes(type)) return {error: JSON.stringify(k) + ' is a ' + type + ' input, not a field to fill'};
    let target = el;
    if (type === 'radio' && typeof v === 'string' && el.name) {
      target = [...document.querySelectorAll('input[type=radio]')].find(x => x.name === el.name && x.value === v);
      if (!target) return {error: JSON.stringify(k) + ': no radio button with value ' + JSON.stringify(v)};
    }
    if (tag === 'select' && ![...el.options].some(o => o.value === String(v) || o.text.trim() === String(v)))
      return {error: JSON.stringify(k) + ': no option ' + JSON.stringify(v)};
    plan.push([k, target, v]);
  }
  for (const [k, el, v] of plan) {
    const type = (el.type || '').toLowerCase();
    if (type === 'checkbox') el.checked = (v === true || v === 'true' || v === 'on' || v === el.value);
    else if (type === 'radio') el.checked = true;
    else {
      let val = String(v);
      if (el.tagName === 'SELECT') val = [...el.options].find(o => o.value === val || o.text.trim() === val).value;
      const d = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value');
      if (d && d.set) d.set.call(el, val); else el.value = val;
    }
    el.dispatchEvent(new Event('input', {bubbles: true}));
    el.dispatchEvent(new Event('change', {bubbles: true}));
  }
  const set = plan.map(p => p[0]);
  if (!submit) return {set, submitted: false, url: location.href};
  const f = plan[0][1].form;
  if (!f) return {error: 'the fields were set but are in no form: nothing submitted', set};
  window.__bitsact_before = 1;
  if (f.requestSubmit) f.requestSubmit(); else f.submit();
  return {set, submitted: true, url: location.href};
})"#;

/// A new document (no marker of FILL) that has loaded.
pub const NEW_DOC_LOADED: &str = "typeof window.__bitsact_before === 'undefined' && document.readyState === 'complete'";
