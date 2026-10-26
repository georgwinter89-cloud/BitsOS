// injected before YouTube loads: hides formats above CAP_H lines and, if BLOCK is set, codecs matching BLOCK
(() => {
  const CAP_H = __CAP_H__, BLOCK = __BLOCK__;
  window.__q = [];
  const ok = (t) => {
    const h = /height=(\d+)/.exec(t), w = /width=(\d+)/.exec(t);
    if (h && +h[1] > CAP_H) return false;
    if (w && +w[1] > Math.round(CAP_H * 16 / 9)) return false;
    if (BLOCK && new RegExp(BLOCK).test(t)) return false;
    return true;
  };
  const wrap = (obj, name) => {
    const orig = obj[name].bind(obj);
    obj[name] = (t) => { const r = ok(t) ? orig(t) : (name === 'canPlayType' ? '' : false); if (window.__q.length < 400) window.__q.push(t + ' => ' + r); return r; };
  };
  wrap(MediaSource, 'isTypeSupported');
  const cpt = HTMLMediaElement.prototype.canPlayType;
  HTMLMediaElement.prototype.canPlayType = function (t) { return ok(t) ? cpt.call(this, t) : ''; };
  if (navigator.mediaCapabilities) {
    const di = navigator.mediaCapabilities.decodingInfo.bind(navigator.mediaCapabilities);
    navigator.mediaCapabilities.decodingInfo = (c) => {
      const v = c && c.video;
      if (v && (v.height > CAP_H || (BLOCK && new RegExp(BLOCK).test(v.contentType))))
        return Promise.resolve({supported: false, smooth: false, powerEfficient: false});
      return di(c);
    };
  }
})();
