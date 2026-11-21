import { ActionIcon, Button, Group, NumberInput, SegmentedControl, Select, Slider, Stack, Switch, Text, Textarea, TextInput, Tooltip } from '@mantine/core';
import {
  IconBraces,
  IconClock,
  IconDice5,
  IconFilter,
  IconHourglass,
  IconListNumbers,
  IconLock,
  IconPlugConnectedX,
  IconPlus,
  IconRoute2,
  IconTrash,
  IconWebhook,
  IconWorld,
} from '@tabler/icons-react';
import { useId, type ReactNode } from 'react';
import { FAULTS, type Fault } from '../../api/types';
import { Copy } from '../../components/common';
import {
  BEHAVIOUR_TITLES,
  CONFLICTS,
  failureShare,
  pathParams,
  type AuthKind,
  type BehaviourKey,
  type Behaviours,
  type ConditionOp,
  type ConditionSource,
  type Issue,
  type Num,
  type QuickForm,
} from '../../lib/quickMock';
import classes from '../QuickMock.module.css';

type Patch = <K extends BehaviourKey>(k: K, patch: Partial<Behaviours[K]>) => void;

const numValue = (v: string | number): Num => (typeof v === 'number' ? v : '');

const FAULT_LABELS: Record<Fault, string> = {
  CONNECTION_RESET: 'Connection reset',
  EMPTY_RESPONSE: 'Empty response (connection closes, no bytes)',
  MALFORMED_JSON: 'Malformed JSON body',
  RANDOM_DATA_THEN_CLOSE: 'Random bytes, then close',
  MALFORMED_RESPONSE_CHUNK: 'Broken chunked encoding',
};

const FAULT_OPTIONS = FAULTS.map((f) => ({ value: f, label: FAULT_LABELS[f] }));

const SOURCES: { value: ConditionSource; label: string }[] = [
  { value: 'header', label: 'Header' },
  { value: 'query', label: 'Query parameter' },
  { value: 'jsonPath', label: 'JSON body field' },
  { value: 'body', label: 'Body text' },
];

const OPS: { value: ConditionOp; label: string }[] = [
  { value: 'equalTo', label: 'equals' },
  { value: 'contains', label: 'contains' },
  { value: 'matches', label: 'matches regex' },
  { value: 'absent', label: 'is absent' },
];

const AUTH_KINDS: { value: AuthKind; label: string }[] = [
  { value: 'bearer', label: 'Any bearer token' },
  { value: 'bearerExact', label: 'One exact bearer token' },
  { value: 'apiKey', label: 'API key header' },
  { value: 'basic', label: 'Basic auth (user and password)' },
];

const HELPERS: { expr: string; what: string }[] = [
  { expr: '{{uuid}}', what: 'random UUID' },
  { expr: '{{now}}', what: 'current time, ISO 8601' },
  { expr: "{{now offset='1 days' format='yyyy-MM-dd'}}", what: 'tomorrow' },
  { expr: '{{randomInt 1 100}}', what: 'random integer' },
  { expr: '{{randomFullName}}', what: 'fake name' },
  { expr: '{{randomEmail}}', what: 'fake e-mail (example.com)' },
  { expr: '{{request.query.page}}', what: 'a query parameter' },
  { expr: '{{request.headers.x-request-id}}', what: 'a request header (lower case)' },
  { expr: '{{request.json.name}}', what: 'a field of the JSON body' },
  { expr: '{{json request.json.amount}}', what: 'keep the JSON type (number, object)' },
];

function Card({
  k,
  icon,
  description,
  on,
  onToggle,
  blockedBy,
  issues,
  children,
}: {
  k: BehaviourKey;
  icon: ReactNode;
  description: string;
  on: boolean;
  onToggle: (on: boolean) => void;
  blockedBy: BehaviourKey | null;
  issues: Issue[];
  children: ReactNode;
}) {
  const id = useId();
  const title = BEHAVIOUR_TITLES[k];
  const blocked = !on && blockedBy !== null;
  const errors = issues.filter((i) => i.field === k || i.field.startsWith(`${k}.`));
  return (
    <div className={classes.card} data-on={on || undefined} data-invalid={errors.length > 0 || undefined} data-testid={`behaviour-${k}`}>
      <div className={classes.cardHead}>
        <span className={classes.cardIcon} aria-hidden>
          {icon}
        </span>
        <label htmlFor={`${id}-switch`} className={classes.cardText}>
          <Text fw={600} size="sm" component="span" display="block">
            {title}
          </Text>
          <Text size="xs" c="dimmed" component="span" display="block" id={`${id}-desc`}>
            {description}
          </Text>
        </label>
        <Tooltip label={blockedBy ? `Turn off ${BEHAVIOUR_TITLES[blockedBy]} first` : ''} disabled={!blocked} withArrow events={{ hover: true, focus: true, touch: true }}>
          <span tabIndex={blocked ? 0 : -1} className={classes.switchWrap}>
            <Switch id={`${id}-switch`} checked={on} disabled={blocked} onChange={(e) => onToggle(e.currentTarget.checked)} aria-describedby={`${id}-desc`} size="md" />
          </span>
        </Tooltip>
      </div>
      {on && (
        <div className={classes.cardBody}>
          {children}
          {errors.map((e, i) => (
            <Text key={i} size="xs" c="var(--mantine-color-error)" role="alert" mt={6}>
              {e.message}
            </Text>
          ))}
        </div>
      )}
    </div>
  );
}

export function BehaviourCards({ form, patch, issues }: { form: QuickForm; patch: Patch; issues: Issue[] }) {
  const b = form.b;
  const blockedBy = (k: BehaviourKey): BehaviourKey | null => (CONFLICTS[k] ?? []).find((o) => b[o].on) ?? null;
  const card = (k: BehaviourKey, icon: ReactNode, description: string, children: ReactNode) => (
    <Card k={k} icon={icon} description={description} on={b[k].on} onToggle={(on) => patch(k, { on })} blockedBy={blockedBy(k)} issues={issues}>
      {children}
    </Card>
  );
  const rows = b.conditions.rows;
  const setRow = (i: number, p: Partial<(typeof rows)[number]>) => patch('conditions', { rows: rows.map((r, j) => (j === i ? { ...r, ...p } : r)) });
  const share = failureShare(b.flaky.percent);
  const params = pathParams(form.path);
  const steps = b.sequence.steps;

  return (
    <div className={classes.cards}>
      {card(
        'conditions',
        <IconFilter size={18} />,
        'Answer only when headers, query parameters or the body match',
        <Stack gap="xs">
          {rows.map((r, i) => (
            <div key={i} className={classes.conditionRow} data-testid="condition-row">
              <div className={classes.condWhat}>
                <Select data={SOURCES} value={r.source} onChange={(v) => v && setRow(i, { source: v })} allowDeselect={false} aria-label={`Condition ${String(i + 1)} source`} size="xs" />
                {r.source !== 'body' ? (
                  <TextInput
                    size="xs"
                    value={r.name}
                    onChange={(e) => setRow(i, { name: e.currentTarget.value })}
                    placeholder={r.source === 'jsonPath' ? '$.customer.id' : r.source === 'header' ? 'X-Client' : 'page'}
                    aria-label={`Condition ${String(i + 1)} ${r.source === 'jsonPath' ? 'JSONPath' : 'name'}`}
                    classNames={{ input: 'mono' }}
                  />
                ) : (
                  <span />
                )}
              </div>
              <div className={classes.condHow}>
                <Select data={OPS} value={r.op} onChange={(v) => v && setRow(i, { op: v })} allowDeselect={false} aria-label={`Condition ${String(i + 1)} operator`} size="xs" />
                {r.op !== 'absent' ? (
                  <TextInput size="xs" value={r.value} onChange={(e) => setRow(i, { value: e.currentTarget.value })} placeholder={r.op === 'matches' ? '[0-9]+' : 'value'} aria-label={`Condition ${String(i + 1)} value`} classNames={{ input: 'mono' }} />
                ) : (
                  <span />
                )}
                <ActionIcon variant="subtle" color="gray" onClick={() => patch('conditions', { rows: rows.filter((_, j) => j !== i) })} aria-label={`Remove condition ${String(i + 1)}`}>
                  <IconTrash size={16} />
                </ActionIcon>
              </div>
            </div>
          ))}
          <Group justify="space-between" wrap="wrap" gap="xs">
            <Button size="compact-sm" variant="subtle" leftSection={<IconPlus size={14} />} onClick={() => patch('conditions', { rows: [...rows, { source: 'header', name: '', op: 'equalTo', value: '' }] })}>
              Add condition
            </Button>
            <Text size="xs" c="dimmed">
              Requests that miss a condition get 404 and show up under Near misses.
            </Text>
          </Group>
        </Stack>,
      )}

      {card(
        'auth',
        <IconLock size={18} />,
        'Expect credentials; a second stub answers 401 without them',
        <div className="fm-form-grid">
          <Select label="Credentials" data={AUTH_KINDS} value={b.auth.kind} onChange={(v) => v && patch('auth', { kind: v })} allowDeselect={false} className="fm-span-2" />
          {b.auth.kind === 'bearerExact' && <TextInput label="Token" value={b.auth.token} onChange={(e) => patch('auth', { token: e.currentTarget.value })} classNames={{ input: 'mono' }} />}
          {b.auth.kind === 'apiKey' && (
            <>
              <TextInput label="Header" value={b.auth.header} onChange={(e) => patch('auth', { header: e.currentTarget.value })} classNames={{ input: 'mono' }} />
              <TextInput label="Key" value={b.auth.key} onChange={(e) => patch('auth', { key: e.currentTarget.value })} classNames={{ input: 'mono' }} />
            </>
          )}
          {b.auth.kind === 'basic' && (
            <>
              <TextInput label="User name" value={b.auth.username} onChange={(e) => patch('auth', { username: e.currentTarget.value })} />
              <TextInput label="Password" value={b.auth.password} onChange={(e) => patch('auth', { password: e.currentTarget.value })} />
            </>
          )}
          <Text size="xs" c="dimmed" className="fm-span-all">
            {b.auth.kind === 'bearer' ? 'Any request with "Authorization: Bearer <something>" gets the response; the token itself is not checked.' : 'FastMock compares the value; it does not verify signatures or expiry.'}
          </Text>
        </div>,
      )}

      {card(
        'delay',
        <IconClock size={18} />,
        'Answer slowly, to test timeouts and spinners',
        <Stack gap="xs">
          <SegmentedControl
            value={b.delay.kind}
            onChange={(v) => patch('delay', { kind: v })}
            data={[
              { label: 'Fixed', value: 'fixed' },
              { label: 'Random range', value: 'range' },
              { label: 'Realistic', value: 'realistic' },
            ]}
            aria-label="Delay type"
            size="xs"
            w="fit-content"
          />
          <div className="fm-form-grid">
            {b.delay.kind === 'fixed' && <NumberInput label="Delay" suffix=" ms" min={0} value={b.delay.fixedMs} onChange={(v) => patch('delay', { fixedMs: numValue(v) })} />}
            {b.delay.kind === 'range' && (
              <>
                <NumberInput label="At least" suffix=" ms" min={0} value={b.delay.minMs} onChange={(v) => patch('delay', { minMs: numValue(v) })} />
                <NumberInput label="At most" suffix=" ms" min={0} value={b.delay.maxMs} onChange={(v) => patch('delay', { maxMs: numValue(v) })} />
              </>
            )}
            {b.delay.kind === 'realistic' && (
              <>
                <NumberInput label="Median" suffix=" ms" min={0} value={b.delay.medianMs} onChange={(v) => patch('delay', { medianMs: numValue(v) })} />
                <NumberInput label="Spread (sigma)" description="0.1 tight, 0.5 typical, 1+ bursty" min={0} max={10} step={0.1} decimalScale={2} value={b.delay.sigma} onChange={(v) => patch('delay', { sigma: numValue(v) })} />
              </>
            )}
          </div>
        </Stack>,
      )}

      {card(
        'fault',
        <IconPlugConnectedX size={18} />,
        'Break the connection on every call instead of answering',
        <Select label="Fault" data={FAULT_OPTIONS} value={b.fault.fault} onChange={(v) => v && patch('fault', { fault: v })} allowDeselect={false} />,
      )}

      {card(
        'flaky',
        <IconDice5 size={18} />,
        'Fail a share of the calls at random, to test retries',
        <Stack gap="sm">
          <div>
            <Group justify="space-between" mb={4} wrap="nowrap">
              <Text size="sm" fw={500} id="flaky-share">
                Calls that fail
              </Text>
              <Text size="sm" className="tabular" data-testid="flaky-share">
                {b.flaky.percent}% ({share.failures} of every {share.total})
              </Text>
            </Group>
            <Slider
              value={b.flaky.percent}
              onChange={(v) => patch('flaky', { percent: v })}
              min={10}
              max={90}
              step={10}
              label={(v) => `${String(v)}%`}
              marks={[{ value: 10, label: '10%' }, { value: 50, label: '50%' }, { value: 90, label: '90%' }]}
              aria-labelledby="flaky-share"
              thumbLabel="Calls that fail"
              mb="md"
            />
          </div>
          <SegmentedControl
            value={b.flaky.failWith}
            onChange={(v) => patch('flaky', { failWith: v })}
            data={[
              { label: 'Error status', value: 'status' },
              { label: 'Network fault', value: 'fault' },
            ]}
            aria-label="How a call fails"
            size="xs"
            w="fit-content"
          />
          {b.flaky.failWith === 'status' ? (
            <div className="fm-form-grid">
              <NumberInput label="Status" min={200} max={599} value={b.flaky.status} onChange={(v) => patch('flaky', { status: numValue(v) })} />
              <Textarea label="Body" value={b.flaky.body} onChange={(e) => patch('flaky', { body: e.currentTarget.value })} autosize minRows={2} maxRows={6} classNames={{ input: 'mono' }} className="fm-span-2" />
            </div>
          ) : (
            <Select label="Fault" data={FAULT_OPTIONS} value={b.flaky.fault} onChange={(v) => v && patch('flaky', { fault: v })} allowDeselect={false} />
          )}
          <Text size="xs" c="dimmed">
            FastMock picks one of {share.total} responses at random on every call; {share.failures} of them fail.
          </Text>
        </Stack>,
      )}

      {card(
        'templating',
        <IconBraces size={18} />,
        'Fill {{…}} placeholders with request data, ids and times',
        <Stack gap={6}>
          <Text size="xs" c="dimmed">
            Copy a placeholder into the body or a header value. Turn this off to send {'{{'} literally.
          </Text>
          <div className={classes.helpers}>
            {[...params.map((p) => ({ expr: `{{request.pathParams.${p}}}`, what: `path parameter ${p}` })), ...HELPERS].map((h) => (
              <div key={h.expr} className={classes.helper}>
                <code className={classes.helperExpr}>{h.expr}</code>
                <Text size="xs" c="dimmed" className={classes.helperWhat}>
                  {h.what}
                </Text>
                <Copy value={h.expr} label={`Copy ${h.expr}`} />
              </div>
            ))}
          </div>
        </Stack>,
      )}

      {card(
        'sequence',
        <IconListNumbers size={18} />,
        'Answer differently on each call: pending, then done',
        <Stack gap="xs">
          <SegmentedControl
            value={b.sequence.mode}
            onChange={(v) => patch('sequence', { mode: v })}
            data={[
              { label: 'Then keep the last', value: 'sequential' },
              { label: 'Start over', value: 'cycle' },
            ]}
            aria-label="After the last response"
            size="xs"
            w="fit-content"
          />
          <Text size="xs" c="dimmed">
            Call 1 gets the response from step 1. The counter is shared by all clients and restarts when the stub changes.
          </Text>
          {steps.map((s, i) => (
            <div key={i} className={classes.stepRow} data-testid="sequence-step">
              <Text size="sm" fw={600} className={classes.stepNo}>
                {i + 2}
              </Text>
              <NumberInput aria-label={`Call ${String(i + 2)} status`} min={200} max={599} value={s.status} onChange={(v) => patch('sequence', { steps: steps.map((x, j) => (j === i ? { ...x, status: numValue(v) } : x)) })} w={96} size="xs" />
              <Textarea aria-label={`Call ${String(i + 2)} body`} value={s.body} onChange={(e) => patch('sequence', { steps: steps.map((x, j) => (j === i ? { ...x, body: e.currentTarget.value } : x)) })} autosize minRows={1} maxRows={6} size="xs" classNames={{ input: 'mono' }} className={classes.stepBody} />
              <ActionIcon variant="subtle" color="gray" onClick={() => patch('sequence', { steps: steps.filter((_, j) => j !== i) })} aria-label={`Remove call ${String(i + 2)}`}>
                <IconTrash size={16} />
              </ActionIcon>
            </div>
          ))}
          <Button size="compact-sm" variant="subtle" leftSection={<IconPlus size={14} />} w="fit-content" onClick={() => patch('sequence', { steps: [...steps, { status: 200, body: '' }] })}>
            Add response
          </Button>
        </Stack>,
      )}

      {card(
        'scenario',
        <IconRoute2 size={18} />,
        'Match only in one state and move the flow to the next',
        <div className="fm-form-grid">
          <TextInput label="Scenario" description="May use {{…}}, e.g. order-{{request.pathParams.id}}" value={b.scenario.name} onChange={(e) => patch('scenario', { name: e.currentTarget.value })} classNames={{ input: 'mono' }} />
          <TextInput label="Only in state" description="Every scenario begins in Started" placeholder="any state" value={b.scenario.requiredState} onChange={(e) => patch('scenario', { requiredState: e.currentTarget.value })} />
          <TextInput label="Then move to" placeholder="stay" value={b.scenario.newState} onChange={(e) => patch('scenario', { newState: e.currentTarget.value })} />
        </div>,
      )}

      {card(
        'callback',
        <IconWebhook size={18} />,
        'Call another URL after answering, with retries',
        <div className="fm-form-grid">
          <TextInput label="URL" value={b.callback.url} onChange={(e) => patch('callback', { url: e.currentTarget.value })} classNames={{ input: 'mono' }} className="fm-span-all" description="Never a loopback address; the server's egress policy applies" />
          <Select label="Method" data={['POST', 'PUT', 'PATCH', 'GET', 'DELETE']} value={b.callback.method} onChange={(v) => v && patch('callback', { method: v })} allowDeselect={false} />
          <NumberInput label="After" suffix=" ms" min={0} value={b.callback.delayMs} onChange={(v) => patch('callback', { delayMs: numValue(v) })} />
          <NumberInput label="Attempts" min={1} max={50} value={b.callback.maxAttempts} onChange={(v) => patch('callback', { maxAttempts: numValue(v) })} />
          <Textarea label="Body" description="JSON or text; {{…}} placeholders see the request" value={b.callback.body} onChange={(e) => patch('callback', { body: e.currentTarget.value })} autosize minRows={3} maxRows={10} classNames={{ input: 'mono' }} className="fm-span-all" />
        </div>,
      )}

      {card(
        'cors',
        <IconWorld size={18} />,
        'Let browser apps, and Try it on this page, call the mock',
        <Stack gap={6}>
          <TextInput label="Allowed origin" description="* allows any page; or e.g. https://app.example.com" value={b.cors.origin} onChange={(e) => patch('cors', { origin: e.currentTarget.value })} classNames={{ input: 'mono' }} maw={420} />
          <Text size="xs" c="dimmed">
            Adds the CORS headers to the response. When the browser has to ask first (a method other than GET or POST, custom headers or a JSON body), a second stub answers its OPTIONS preflight.
          </Text>
        </Stack>,
      )}

      {card(
        'limits',
        <IconHourglass size={18} />,
        'Remove the mock after some calls or some time',
        <div className="fm-form-grid">
          <NumberInput label="Answer at most" suffix=" calls" min={1} value={b.limits.times} onChange={(v) => patch('limits', { times: numValue(v) })} placeholder="no limit" />
          <NumberInput label="Expire after" suffix=" min" min={1} max={525600} value={b.limits.ttlMinutes} onChange={(v) => patch('limits', { ttlMinutes: numValue(v) })} placeholder="never" />
        </div>,
      )}
    </div>
  );
}
