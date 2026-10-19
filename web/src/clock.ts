// A clock that keeps ticking in a hidden tab: browsers throttle the page's own timers there
// (to once a minute, in time), past the daemon's 45 s limit for a quiet client; a worker's
// messages still arrive.
let timer: ReturnType<typeof setInterval> | undefined;
self.onmessage = (ev: MessageEvent<number>) => {
  clearInterval(timer);
  timer = setInterval(() => self.postMessage(0), ev.data);
};
