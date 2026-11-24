// A tiny, safe markdown renderer for detail panes: headings, bullets, fences, `code`, **bold**.
// It builds React elements only; untrusted text never becomes HTML.
import type { ReactNode } from 'react';

function inline(text: string, key: string): ReactNode[] {
  const out: ReactNode[] = [];
  const re = /(`[^`]+`|\*\*[^*]+\*\*)/g;
  let last = 0;
  let m: RegExpExecArray | null;
  let i = 0;
  while ((m = re.exec(text))) {
    if (m.index > last) out.push(text.slice(last, m.index));
    const tok = m[0];
    out.push(tok.startsWith('`') ? <code key={`${key}-${i++}`}>{tok.slice(1, -1)}</code> : <strong key={`${key}-${i++}`}>{tok.slice(2, -2)}</strong>);
    last = m.index + tok.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

export function Markdown({ text }: { text: string }) {
  const lines = text.split('\n');
  const blocks: ReactNode[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    const fence = /^\s{0,3}(`{3,}|~{3,})/.exec(line);
    if (fence) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i].trim().startsWith(fence[1])) body.push(lines[i++]);
      i++;
      blocks.push(<pre key={i}>{body.join('\n')}</pre>);
      continue;
    }
    if (/^\s*[-*] /.test(line)) {
      const items: string[] = [];
      while (i < lines.length && /^\s*[-*] /.test(lines[i])) items.push(lines[i++].replace(/^\s*[-*] /, ''));
      blocks.push(<ul key={i}>{items.map((t, j) => <li key={j}>{inline(t, `${i}-${j}`)}</li>)}</ul>);
      continue;
    }
    const h = /^(#{1,4})\s+(.*)$/.exec(line);
    if (h) blocks.push(<h4 key={i}>{inline(h[2], `h${i}`)}</h4>);
    else if (line.trim()) blocks.push(<p key={i}>{inline(line, `p${i}`)}</p>);
    i++;
  }
  return <div className="md">{blocks}</div>;
}
