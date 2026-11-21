import type { ChaosStage, MailChaosRule, MailStringMatcher } from '../api/mail';
import { cleanRule, MATCH_FIELDS, simpleMatcher, type MatchField, type MatcherOp } from './mail';

/** The editable form of a mail chaos rule. */
export type Effect = 'reply' | 'disconnect' | 'providerError' | 'delay';

export interface MatchDraft {
  op: MatcherOp;
  value: string;
  caseInsensitive: boolean;
  /** A matcher the form cannot edit (and, or, ...); kept as it is unless cleared. */
  custom?: MailStringMatcher;
}

export interface ChaosDraft {
  id: string;
  stage: ChaosStage;
  match: Record<MatchField, MatchDraft>;
  effect: Effect;
  code: number | '';
  enhanced: string;
  text: string;
  delay: string;
  providerError: string;
  times: number | '';
  every: number | '';
  probability: number | '';
  seed: number | '';
}

const NO_MATCH = (): Record<MatchField, MatchDraft> => ({
  rcptTo: { op: 'contains', value: '', caseInsensitive: false },
  mailFrom: { op: 'contains', value: '', caseInsensitive: false },
  authUser: { op: 'equalTo', value: '', caseInsensitive: false },
  provider: { op: 'equalTo', value: '', caseInsensitive: false },
});

export const EMPTY_DRAFT: ChaosDraft = {
  id: '',
  stage: 'rcpt',
  match: NO_MATCH(),
  effect: 'reply',
  code: 451,
  enhanced: '4.3.0',
  text: '',
  delay: '',
  providerError: '',
  times: '',
  every: '',
  probability: '',
  seed: '',
};

function num(v: number | ''): number | undefined {
  return v === '' ? undefined : v;
}

export function draftToRule(d: ChaosDraft): MailChaosRule {
  const match: NonNullable<MailChaosRule['match']> = {};
  for (const { field } of MATCH_FIELDS) {
    const m = d.match[field];
    if (m.custom) match[field] = m.custom;
    else if (m.value !== '') match[field] = { [m.op]: m.value, ...(m.caseInsensitive ? { caseInsensitive: true } : {}) };
  }
  const action: MailChaosRule['action'] = {};
  if (d.effect === 'reply') action.reply = { code: typeof d.code === 'number' ? d.code : 0, enhanced: d.enhanced.trim() || null, text: d.text || null };
  if (d.effect === 'disconnect') action.disconnect = true;
  if (d.effect === 'providerError') action.providerError = d.providerError.trim();
  if (d.delay.trim()) action.delay = d.delay.trim();
  return cleanRule({
    id: d.id.trim() || null,
    stage: d.stage,
    match,
    action,
    times: num(d.times),
    every: num(d.every),
    probability: num(d.probability),
    seed: num(d.seed),
  });
}

export function ruleToDraft(r: MailChaosRule): ChaosDraft {
  const match = NO_MATCH();
  for (const { field } of MATCH_FIELDS) {
    const m = r.match?.[field];
    if (!m) continue;
    const s = simpleMatcher(m);
    match[field] = s ? s : { ...match[field], custom: m };
  }
  const a = r.action;
  const effect: Effect = a.reply ? 'reply' : a.disconnect ? 'disconnect' : a.providerError ? 'providerError' : 'delay';
  return {
    id: r.id ?? '',
    stage: r.stage,
    match,
    effect,
    code: a.reply?.code ?? 451,
    enhanced: a.reply?.enhanced ?? '',
    text: a.reply?.text ?? '',
    delay: a.delay ?? '',
    providerError: a.providerError ?? '',
    times: r.times ?? '',
    every: r.every ?? '',
    probability: r.probability ?? '',
    seed: r.seed ?? '',
  };
}

