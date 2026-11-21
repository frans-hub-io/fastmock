import { Tooltip } from '@mantine/core';
import { useElementSize } from '@mantine/hooks';
import { useState, type ReactNode } from 'react';
import { compact, niceTicks, type Bucket } from '../lib/traffic';
import classes from './charts.module.css';

const clock = (t: number) => new Date(t).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', hour12: false });

/** A path for a bar of width `w` and height `h` at (x, y) with rounded top corners and a square base. */
function topRounded(x: number, y: number, w: number, h: number, r: number): string {
  const rr = Math.max(0, Math.min(r, w / 2, h));
  return `M${x},${y + h}V${y + rr}Q${x},${y} ${x + rr},${y}H${x + w - rr}Q${x + w},${y} ${x + w},${y + rr}V${y + h}Z`;
}

/**
 * Requests over time as stacked columns, matched below and unmatched above, with a crosshair tooltip on hover
 * or keyboard focus (arrow keys) and the numbers in a visually hidden table for screen readers.
 */
export function VolumeChart({ buckets, height = 190, bucketMinutes }: { buckets: Bucket[]; height?: number; bucketMinutes: number }) {
  const { ref, width } = useElementSize<HTMLDivElement>();
  const [active, setActive] = useState<number | null>(null);
  const n = buckets.length;
  const max = Math.max(1, ...buckets.map((b) => b.matched + b.unmatched));
  const ticks = niceTicks(max, 3);
  const top = ticks[ticks.length - 1] ?? max;
  const pad = { l: 34, r: 6, t: 8, b: 22 };
  const w = Math.max(0, width);
  const pw = Math.max(1, w - pad.l - pad.r);
  const ph = height - pad.t - pad.b;
  const slot = pw / Math.max(1, n);
  const bw = Math.max(2, Math.min(24, slot - 2));
  const y = (v: number) => pad.t + ph - (v / top) * ph;
  const cur = active === null ? null : buckets[active];
  const tipLeft = active === null ? 0 : pad.l + active * slot + slot / 2;

  return (
    <div className={classes.volume}>
      <div
        ref={ref}
        className={classes.plot}
        tabIndex={0}
        role="img"
        aria-label={`Requests per ${String(bucketMinutes)} minutes over the last hour: ${String(buckets.reduce((s, b) => s + b.matched + b.unmatched, 0))} in total. Use the arrow keys to read each interval.`}
        onKeyDown={(e) => {
          if (e.key === 'ArrowLeft') setActive((a) => Math.max(0, (a ?? n) - 1));
          else if (e.key === 'ArrowRight') setActive((a) => Math.min(n - 1, (a ?? -1) + 1));
          else if (e.key === 'Escape') setActive(null);
        }}
        onFocus={() => setActive((a) => a ?? n - 1)}
        onBlur={() => setActive(null)}
        onMouseLeave={() => setActive(null)}
      >
        {w > 0 && (
          <svg width={w} height={height} className={classes.svg} aria-hidden>
            {ticks.map((t) => (
              <g key={t}>
                <line x1={pad.l} x2={w - pad.r} y1={y(t)} y2={y(t)} className={classes.grid} />
                <text x={pad.l - 6} y={y(t)} dy="0.32em" textAnchor="end" className={classes.tick}>
                  {compact(t)}
                </text>
              </g>
            ))}
            {active !== null && <line x1={tipLeft} x2={tipLeft} y1={pad.t} y2={pad.t + ph} className={classes.cross} />}
            {buckets.map((b, i) => {
              const x = pad.l + i * slot + (slot - bw) / 2;
              const hm = (b.matched / top) * ph;
              const hu = (b.unmatched / top) * ph;
              const base = pad.t + ph;
              const gap = hm > 0 && hu > 0 ? 2 : 0;
              const dim = active !== null && active !== i;
              return (
                <g key={b.start} className={dim ? classes.dim : undefined}>
                  {hm > 0 && (hu > 0 ? <rect x={x} y={base - hm} width={bw} height={hm} className={classes.s1} /> : <path d={topRounded(x, base - hm, bw, hm, 4)} className={classes.s1} />)}
                  {hu > 0 && <path d={topRounded(x, base - hm - hu, bw, Math.max(1, hu - gap), 4)} className={classes.s2} />}
                  <rect x={pad.l + i * slot} y={pad.t} width={slot} height={ph} className={classes.hit} onMouseEnter={() => setActive(i)} />
                </g>
              );
            })}
            <line x1={pad.l} x2={w - pad.r} y1={pad.t + ph} y2={pad.t + ph} className={classes.axis} />
            {[0, Math.floor(n / 2)].map((i) =>
              buckets[i] ? (
                <text key={i} x={pad.l + i * slot + slot / 2} y={height - 6} textAnchor={i === 0 ? 'start' : 'middle'} className={classes.tick}>
                  {clock(buckets[i].start)}
                </text>
              ) : null,
            )}
            <text x={w - pad.r} y={height - 6} textAnchor="end" className={classes.tick}>
              now
            </text>
          </svg>
        )}
        {cur && (
          <div className={classes.tip} data-side={tipLeft > w / 2 ? 'left' : 'right'} style={{ left: tipLeft }}>
            <div className={classes.tipTime}>
              {clock(cur.start)}–{clock(cur.start + bucketMinutes * 60_000)}
            </div>
            <div className={classes.tipRow}>
              <span className={`${classes.key} ${classes.k1}`} />
              <strong>{cur.matched}</strong> matched
            </div>
            <div className={classes.tipRow}>
              <span className={`${classes.key} ${classes.k2}`} />
              <strong>{cur.unmatched}</strong> unmatched
            </div>
          </div>
        )}
      </div>
      <table className="sr-only">
        <caption>Requests per interval</caption>
        <thead>
          <tr>
            <th>From</th>
            <th>Matched</th>
            <th>Unmatched</th>
          </tr>
        </thead>
        <tbody>
          {buckets.map((b) => (
            <tr key={b.start}>
              <td>{clock(b.start)}</td>
              <td>{b.matched}</td>
              <td>{b.unmatched}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function Legend({ items }: { items: { label: string; series: 1 | 2; value?: ReactNode }[] }) {
  return (
    <div className={classes.legend}>
      {items.map((i) => (
        <span key={i.label} className={classes.legendItem}>
          <span className={`${classes.swatch} ${i.series === 1 ? classes.k1 : classes.k2}`} aria-hidden />
          {i.label}
          {i.value !== undefined && <strong className={classes.legendValue}>{i.value}</strong>}
        </span>
      ))}
    </div>
  );
}

/** A single-series trend line with a soft area wash and an end dot; decorative next to a stat. */
export function Sparkline({ values, height = 36, series = 1 }: { values: number[]; height?: number; series?: 1 | 2 }) {
  const { ref, width } = useElementSize<HTMLDivElement>();
  const n = values.length;
  const max = Math.max(1, ...values);
  const pts = values.map((v, i) => [n <= 1 ? 0 : (i / (n - 1)) * Math.max(0, width - 8) + 4, 4 + (height - 8) * (1 - v / max)] as const);
  const line = pts.map(([x, y], i) => `${i ? 'L' : 'M'}${x.toFixed(1)},${y.toFixed(1)}`).join('');
  const last = pts[pts.length - 1];
  return (
    <div ref={ref} className={classes.spark} aria-hidden>
      {width > 0 && n > 1 && last && (
        <svg width={width} height={height}>
          <path d={`${line}L${last[0].toFixed(1)},${String(height)}L4,${String(height)}Z`} className={series === 1 ? classes.wash1 : classes.wash2} />
          <path d={line} className={series === 1 ? classes.line1 : classes.line2} />
          <circle cx={last[0]} cy={last[1]} r={3.5} className={series === 1 ? classes.dot1 : classes.dot2} />
        </svg>
      )}
    </div>
  );
}

export interface Segment {
  key: string;
  label: string;
  value: number;
  tone: 'success' | 'info' | 'warning' | 'danger' | 'neutral';
}

/** One whole split into parts: a thin bar with a gap between parts and a legend that names every part. */
export function SegmentBar({ parts, label, testId }: { parts: Segment[]; label: string; testId?: string }) {
  const total = parts.reduce((s, p) => s + p.value, 0);
  const pct = (v: number) => (total ? Math.round((v / total) * 100) : 0);
  return (
    <div className={classes.segments} data-testid={testId}>
      <div className={classes.segBar} role="img" aria-label={`${label}: ${parts.map((p) => `${p.label} ${String(p.value)}`).join(', ')}`}>
        {total === 0 ? (
          <span className={classes.segEmpty} />
        ) : (
          parts
            .filter((p) => p.value > 0)
            .map((p) => (
              <Tooltip key={p.key} label={`${p.label}: ${String(p.value)} (${String(pct(p.value))}%)`}>
                <span className={classes.seg} data-tone={p.tone} style={{ flexGrow: p.value }} />
              </Tooltip>
            ))
        )}
      </div>
      <div className={classes.segLegend}>
        {parts.map((p) => (
          <div key={p.key} className={classes.segItem}>
            <span className={classes.segDot} data-tone={p.tone} aria-hidden />
            <span className={classes.segLabel}>{p.label}</span>
            <span className={classes.segValue}>{compact(p.value)}</span>
            <span className={classes.segPct}>{total ? `${String(pct(p.value))}%` : '—'}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

/** Ranked values as horizontal bars, the value at the tip. */
export function BarList({ rows, testId }: { rows: { key: string; label: ReactNode; value: number; hint?: ReactNode }[]; testId?: string }) {
  const max = Math.max(1, ...rows.map((r) => r.value));
  return (
    <ol className={classes.barList} data-testid={testId}>
      {rows.map((r) => (
        <li key={r.key} className={classes.barRow}>
          <div className={classes.barHead}>
            <span className={classes.barLabel}>{r.label}</span>
            <span className={classes.barValue}>{compact(r.value)}</span>
          </div>
          <div className={classes.barTrack}>
            <span className={classes.barFill} style={{ width: `${String(Math.max(2, (r.value / max) * 100))}%` }} />
          </div>
          {r.hint && <div className={classes.barHint}>{r.hint}</div>}
        </li>
      ))}
    </ol>
  );
}

/**
 * Passed, failed and errored checks as a compact bar with the passed share as text, for a table cell; the bar
 * is decorative and the label says the numbers.
 */
export function ResultBar({ passed, failed, errors, total }: { passed: number; failed: number; errors: number; total: number }) {
  const parts: { tone: Segment['tone']; value: number }[] = [
    { tone: 'success', value: passed },
    { tone: 'danger', value: failed },
    { tone: 'warning', value: errors },
  ];
  const pending = Math.max(0, total - passed - failed - errors);
  return (
    <span className={classes.result} role="img" aria-label={`${String(passed)} of ${String(total)} passed, ${String(failed)} failed, ${String(errors)} errors`}>
      <span className={classes.resultBar} aria-hidden>
        {total === 0 ? (
          <span className={classes.segEmpty} />
        ) : (
          <>
            {parts
              .filter((p) => p.value > 0)
              .map((p) => (
                <span key={p.tone} className={classes.seg} data-tone={p.tone} style={{ flexGrow: p.value }} />
              ))}
            {pending > 0 && <span className={classes.seg} data-tone="neutral" style={{ flexGrow: pending }} />}
          </>
        )}
      </span>
      <span className={classes.resultText} aria-hidden>
        {passed}/{total}
      </span>
    </span>
  );
}
