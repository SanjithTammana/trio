// View-transition morphs for the detail panel and the island.

/** Pointer-initiated opens morph from their source; keyboard ones (event.detail === 0) and
 * reduced-motion users get an instant change. Only one element may own `morph` at a time. */
export function morph(source: Element | null | undefined, pointer: boolean, update: () => void) {
  const doc = document as Document & { startViewTransition?: (cb: () => void) => { finished: Promise<void> } };
  const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const src = source as HTMLElement | SVGElement | null | undefined;
  if (!doc.startViewTransition || !pointer || reduce || !src || document.querySelector('.panel')) {
    update();
    return;
  }
  src.style.viewTransitionName = 'morph';
  doc.startViewTransition(() => {
    src.style.viewTransitionName = '';
    update();
  });
}

/** The reverse: the panel collapses back into the element it came from, if it is still on screen. */
export function unmorph(targetSelector: string | null, pointer: boolean, update: () => void) {
  const doc = document as Document & { startViewTransition?: (cb: () => void) => { finished: Promise<void> } };
  const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (!doc.startViewTransition || !pointer || reduce) {
    update();
    return;
  }
  let target: HTMLElement | null = null;
  const t = doc.startViewTransition(() => {
    update();
    target = targetSelector ? document.querySelector<HTMLElement>(targetSelector) : null;
    if (target) target.style.viewTransitionName = 'morph';
  });
  t.finished.finally(() => { if (target) target.style.viewTransitionName = ''; });
}
