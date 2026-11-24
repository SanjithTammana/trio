/*! Segmented control ported from Astryx (https://github.com/meta/astryx), @astryxdesign/core 0.7.17.
 * MIT License. Copyright (c) 2026 Meta Platforms, Inc.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */
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
  // Re-measure when the group resizes too (window, font load), or the indicator goes stale.
  useLayoutEffect(() => {
    const measure = () => {
      const el = list.current?.querySelector<HTMLElement>('[role="radio"][aria-checked="true"]');
      if (el) setPill({ left: el.offsetLeft, width: el.offsetWidth });
    };
    measure();
    if (!list.current || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(measure);
    ro.observe(list.current);
    return () => ro.disconnect();
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
