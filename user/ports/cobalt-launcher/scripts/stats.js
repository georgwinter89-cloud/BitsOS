(() => { const v = document.querySelector('video'); const o = {};
 if (v) { const q = v.getVideoPlaybackQuality ? v.getVideoPlaybackQuality() : {};
  const b = []; for (let i = 0; i < v.buffered.length; i++) b.push([+v.buffered.start(i).toFixed(1), +v.buffered.end(i).toFixed(1)]);
  Object.assign(o, {t: +v.currentTime.toFixed(2), paused: v.paused, w: v.videoWidth, h: v.videoHeight, rs: v.readyState, ns: v.networkState,
   buf: b, src: (v.src||'').slice(-8), total: q.totalVideoFrames, dropped: q.droppedVideoFrames}); }
 return o; })()
