// Module-level singleton wrapping `captureDomRefs()`. Replaces the
// per-deps `dom` field that every extracted module used to take through
// its setup closure. `initDom()` must be called once in `bootstrap()`
// before any module accesses `dom()`; the assert guards against ordering
// regressions during the singleton refactor.

import { captureDomRefs, type DomRefs } from "./domRefs";

let _dom: DomRefs | null = null;

export function initDom(): void {
  if (_dom === null) {
    _dom = captureDomRefs();
  }
}

export function dom(): DomRefs {
  if (_dom === null) {
    throw new Error("dom() accessed before initDom()");
  }
  return _dom;
}
