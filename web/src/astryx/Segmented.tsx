// Ported from Astryx (@astryxdesign/core 0.7.17, SegmentedControl), MIT, Copyright (c) Meta
// Platforms, Inc. and affiliates. See ./LICENSE.
//
// Kept from Astryx: the WAI-ARIA radiogroup contract (one Tab stop with a roving tabindex;
// Arrow keys, Home and End move focus and select within the group; the ends wrap) and a
// selection indicator that slides from the old segment to the new one. Left out: touch
// tracking, the spring animator, i18n and StyleX, which Trio's single CSS file replaces.
import { useLayoutEffect, useRef, useState, type KeyboardEvent } from 'react';

export interface Segment<T extends string> { value: T; label: string; hint?: string }

export function Segmented<T extends string>({ label, value, segments, onChange }: { label: string; value: T; segments: Segment<T>[]; onChange: (v: T) => void }) {
  const list = useRef<HTMLDivElement>(null);
  const [pill, setPill] = useState<{ left: number; width: number } | null>(null);

  // Measure the selected segment after layout, so the indicator slides between real positions.
  useLayoutEffect(() => {
    const el = list.current?.querySelector<HTMLElement>('[role="radio"][aria-checked="true"]');
    if (el) setPill({ left: el.offsetLeft, width: el.offsetWidth });
  }, [value, segments.length]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const i = segments.findIndex((s) => s.value === value);
    const n = segments.length;
    const to = { ArrowRight: (i + 1) % n, ArrowDown: (i + 1) % n, ArrowLeft: (i - 1 + n) % n, ArrowUp: (i - 1 + n) % n, Home: 0, End: n - 1 }[e.key];
    if (to === undefined) return;
    e.preventDefault();
    onChange(segments[to].value);
    requestAnimationFrame(() => list.current?.querySelectorAll<HTMLElement>('[role="radio"]')[to]?.focus());
  };

  return (
    <div className="segmented" role="radiogroup" aria-label={label} ref={list} onKeyDown={onKeyDown}>
      {pill && <span className="segmented-pill" style={{ transform: `translateX(${pill.left}px)`, width: pill.width }} aria-hidden />}
      {segments.map((s) => (
        <button key={s.value} type="button" role="radio" aria-checked={s.value === value} tabIndex={s.value === value ? 0 : -1} title={s.hint} onClick={() => onChange(s.value)}>
          {s.label}
        </button>
      ))}
    </div>
  );
}
