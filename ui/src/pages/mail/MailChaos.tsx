import { ActionIcon, Alert, Button, Checkbox, Code, Group, NumberInput, SegmentedControl, Select, Stack, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { IconAlertTriangle, IconArrowDown, IconArrowUp, IconDeviceFloppy, IconFlame, IconPencil, IconPlus, IconRestore, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { mailApi, type MailChaosRule } from '../../api/mail';
import { confirmAction, useApiMutation } from '../../api/mutations';
import { useCan, useSpace, useSpaceQuery } from '../../auth/useAuth';
import { DataTable, SaveBar, SectionCard } from '../../components/common';
import { EmptyState, ErrorState, LoadingState } from '../../components/States';
import { docsUrl } from '../../lib/docs';
import { actionText, cleanRule, firingText, mailChaosIssues, MATCH_FIELDS, matcherText, stageAllows, STAGES, type ChaosIssues, type MatchField } from '../../lib/mail';
import { draftToRule, EMPTY_DRAFT, ruleToDraft, type ChaosDraft, type Effect, type MatchDraft } from '../../lib/mailChaos';
import classes from './Mail.module.css';

const MAX_RULES = 100;

function MatchSummary({ r }: { r: MailChaosRule }) {
  const parts = MATCH_FIELDS.filter((f) => r.match?.[f.field]).map((f) => (
    <span key={f.field} className="fm-chip">
      {f.label.toLowerCase()} {matcherText(r.match?.[f.field])}
    </span>
  ));
  if (!parts.length)
    return (
      <Text size="sm" c="dimmed" component="span">
        every event
      </Text>
    );
  return (
    <Group gap={4} wrap="wrap">
      {parts}
    </Group>
  );
}

function RuleForm({
  draft,
  setDraft,
  issues,
  touched,
  editing,
  onSubmit,
  onCancel,
}: {
  draft: ChaosDraft;
  setDraft: (d: ChaosDraft) => void;
  issues: ChaosIssues;
  touched: boolean;
  editing: boolean;
  onSubmit: () => void;
  onCancel: () => void;
}) {
  const show = (k: keyof ChaosIssues) => (touched || k === 'id' ? issues[k] : undefined);
  const provider = draft.stage === 'provider';
  const effects = provider
    ? [
        { label: 'Provider error', value: 'providerError' },
        { label: 'Delay only', value: 'delay' },
      ]
    : [
        { label: 'Reply', value: 'reply' },
        { label: 'Disconnect', value: 'disconnect' },
        { label: 'Delay only', value: 'delay' },
      ];
  const setMatch = (f: MatchField, m: Partial<MatchDraft>) => setDraft({ ...draft, match: { ...draft.match, [f]: { ...draft.match[f], ...m } } });
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        onSubmit();
      }}
      data-testid="mail-chaos-form"
    >
      <Stack gap="md">
        <div className="fm-form-grid">
          <TextInput label="Id" description="Optional; rule-<n> when empty" placeholder="third-message-fails" value={draft.id} onChange={(e) => setDraft({ ...draft, id: e.currentTarget.value })} error={show('id')} classNames={{ input: 'mono' }} data-testid="chaos-id" />
          <Select
            label="Stage"
            description={STAGES.find((s) => s.value === draft.stage)?.hint}
            data={STAGES.map((s) => ({ value: s.value, label: s.label }))}
            value={draft.stage}
            allowDeselect={false}
            onChange={(v) => {
              const stage = (v ?? 'rcpt');
              const effect: Effect = stage === 'provider' ? (draft.effect === 'delay' ? 'delay' : 'providerError') : draft.effect === 'providerError' ? 'reply' : draft.effect;
              setDraft({ ...draft, stage, effect });
            }}
            data-testid="chaos-stage"
          />
        </div>
        <div>
          <Text size="sm" fw={500} mb={4}>
            Match
          </Text>
          <Text size="xs" c="dimmed" mb="xs">
            All given conditions must hold; leave empty to match every event at this stage.
          </Text>
          <Stack gap="xs">
            {MATCH_FIELDS.filter((f) => stageAllows(draft.stage, f.field) || draft.match[f.field].value || draft.match[f.field].custom).map((f) => {
              const m = draft.match[f.field];
              return (
                <Group key={f.field} gap="xs" wrap="wrap" align="flex-start">
                  <Text size="sm" w={96} pt={6}>
                    {f.label}
                  </Text>
                  {m.custom ? (
                    <Group gap="xs" className="fm-grow" wrap="nowrap">
                      <Code className="break-all fm-grow">{JSON.stringify(m.custom)}</Code>
                      <Button size="compact-sm" variant="subtle" onClick={() => setMatch(f.field, { custom: undefined, value: '' })}>
                        Clear
                      </Button>
                    </Group>
                  ) : (
                    <>
                      <Select
                        aria-label={`${f.label} operator`}
                        data={[
                          { value: 'contains', label: 'contains' },
                          { value: 'equalTo', label: 'equals' },
                          { value: 'matches', label: 'matches regex' },
                        ]}
                        value={m.op}
                        allowDeselect={false}
                        onChange={(v) => setMatch(f.field, { op: (v ?? 'contains') })}
                        w={150}
                      />
                      <TextInput
                        aria-label={`${f.label} value`}
                        placeholder={f.field === 'provider' ? 'sendgrid' : f.field === 'authUser' ? 'billing' : 'slow@'}
                        value={m.value}
                        onChange={(e) => setMatch(f.field, { value: e.currentTarget.value })}
                        className="fm-grow"
                        miw={160}
                        classNames={{ input: 'mono' }}
                        data-testid={`chaos-match-${f.field}`}
                      />
                      <Checkbox label="any case" checked={m.caseInsensitive} onChange={(e) => setMatch(f.field, { caseInsensitive: e.currentTarget.checked })} mt={8} />
                    </>
                  )}
                </Group>
              );
            })}
          </Stack>
          {show('match') && (
            <Text size="xs" c="red" mt={4}>
              {issues.match}
            </Text>
          )}
        </div>
        <div>
          <Text size="sm" fw={500} mb={6}>
            Effect
          </Text>
          <SegmentedControl value={draft.effect} onChange={(v) => setDraft({ ...draft, effect: v as Effect })} data={effects} aria-label="Effect" data-testid="chaos-effect" />
        </div>
        <div className="fm-form-grid">
          {draft.effect === 'reply' && (
            <>
              <NumberInput label="Reply code" value={draft.code} onChange={(v) => setDraft({ ...draft, code: typeof v === 'number' ? v : '' })} min={400} max={599} error={show('code')} data-testid="chaos-code" />
              <TextInput label="Enhanced code" placeholder="4.3.0" value={draft.enhanced} onChange={(e) => setDraft({ ...draft, enhanced: e.currentTarget.value })} error={show('enhanced')} classNames={{ input: 'mono' }} data-testid="chaos-enhanced" />
              <TextInput label="Text" placeholder="Try again later" value={draft.text} onChange={(e) => setDraft({ ...draft, text: e.currentTarget.value })} error={show('text')} className="fm-span-2" />
            </>
          )}
          {draft.effect === 'providerError' && (
            <TextInput
              label="Provider error"
              description="e.g. MessageRejected, rateLimit, invalidKey"
              placeholder="rateLimit"
              value={draft.providerError}
              onChange={(e) => setDraft({ ...draft, providerError: e.currentTarget.value })}
              error={show('providerError')}
              className="fm-span-2"
              classNames={{ input: 'mono' }}
            />
          )}
          <TextInput
            label={draft.effect === 'delay' ? 'Delay' : 'Delay first (optional)'}
            description="500ms, 2s or 1m; at most 10 minutes"
            placeholder="2s"
            value={draft.delay}
            onChange={(e) => setDraft({ ...draft, delay: e.currentTarget.value })}
            error={show('delay')}
            classNames={{ input: 'mono' }}
            data-testid="chaos-delay"
          />
        </div>
        <div>
          <Text size="sm" fw={500} mb={4}>
            When it fires
          </Text>
          <div className="fm-form-grid">
            <NumberInput label="Every n-th event" placeholder="every" value={draft.every} onChange={(v) => setDraft({ ...draft, every: typeof v === 'number' ? v : '' })} min={1} allowDecimal={false} error={show('every')} data-testid="chaos-every" />
            <NumberInput label="At most (times)" placeholder="unlimited" value={draft.times} onChange={(v) => setDraft({ ...draft, times: typeof v === 'number' ? v : '' })} min={1} allowDecimal={false} error={show('times')} data-testid="chaos-times" />
            <NumberInput label="Probability" suffix=" %" placeholder="100 %" value={draft.probability} onChange={(v) => setDraft({ ...draft, probability: typeof v === 'number' ? v : '' })} min={0} max={100} error={show('probability')} data-testid="chaos-probability" />
            <NumberInput label="Seed" description="Repeatable probability" value={draft.seed} onChange={(v) => setDraft({ ...draft, seed: typeof v === 'number' ? v : '' })} min={0} allowDecimal={false} error={show('seed')} data-testid="chaos-seed" />
          </div>
        </div>
        {((touched && issues.action) || issues.limit) && (
          <Alert color="yellow" icon={<IconAlertTriangle size={18} />} p="xs" data-testid="chaos-action-issue">
            {issues.limit ?? issues.action}
          </Alert>
        )}
        <Group justify="flex-end" gap="xs">
          <Button variant="subtle" color="gray" leftSection={<IconRestore size={16} />} onClick={onCancel}>
            {editing ? 'Cancel edit' : 'Reset'}
          </Button>
          <Button type="submit" variant="light" leftSection={editing ? <IconPencil size={16} /> : <IconPlus size={16} />} disabled={!editing && !!issues.limit} data-testid="chaos-add">
            {editing ? 'Update rule' : 'Add rule'}
          </Button>
        </Group>
      </Stack>
    </form>
  );
}

export function MailChaos() {
  const can = useCan();
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const q = useQuery({ queryKey: ['mail', 'chaos', spaceQ.space ?? null], queryFn: () => mailApi.chaos(spaceQ.space) });
  const [rules, setRules] = useState<MailChaosRule[]>([]);
  const [dirty, setDirty] = useState(false);
  const [draft, setDraft] = useState<ChaosDraft>(EMPTY_DRAFT);
  const [editing, setEditing] = useState<number | null>(null);
  const [touched, setTouched] = useState(false);

  useEffect(() => {
    if (q.data && !dirty) setRules(q.data.rules);
  }, [q.data, dirty]);

  const area = q.data ? (q.data.space ?? null) : (spaceQ.space ?? null);
  const canEdit = can('mail:write', area);

  const save = useApiMutation<MailChaosRule[]>({
    fn: (r) => mailApi.putChaos(r.map(cleanRule), area),
    invalidate: [['mail', 'chaos']],
    success: 'Mail chaos rules saved',
    errorTitle: 'The server refused the rules',
    onSuccess: () => setDirty(false),
  });
  const clear = useApiMutation<void>({
    fn: () => mailApi.clearChaos(area),
    invalidate: [['mail', 'chaos']],
    success: 'Mail chaos rules removed',
    onSuccess: () => setDirty(false),
  });

  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;

  const change = (next: MailChaosRule[]) => {
    setRules(next);
    setDirty(true);
  };
  const others = rules.filter((_, i) => i !== editing);
  const rule = draftToRule(draft);
  const issues = mailChaosIssues(rule, others, editing === null ? MAX_RULES : MAX_RULES + 1);
  const blocked = Object.keys(issues).length > 0;
  const counters = new Map((q.data?.counters ?? []).map((c) => [c.id, c]));
  const scope = area ? `space ${area}` : 'the global area';

  const resetForm = () => {
    setDraft(EMPTY_DRAFT);
    setEditing(null);
    setTouched(false);
  };

  return (
    <Stack gap="md">
      <Group justify="space-between" wrap="wrap" gap="xs">
        <Text size="sm" c="dimmed" maw={760}>
          SMTP faults for {scope}: replies, delays and dropped connections at a stage of the session, or provider API errors. Rules are evaluated in order; the first that fires wins. The connect, tls and ehlo
          stages use the global area’s rules{spaces.enabled ? '; auth and mail the space named like the AUTH user; rcpt and data the recipients’ space' : ''}. Without a rule, a recipient like{' '}
          <code>fail+451@example.test</code> is refused with that code.
        </Text>
        {canEdit && (
          <Button
            variant="light"
            color="red"
            leftSection={<IconTrash size={16} />}
            disabled={!q.data?.rules.length}
            onClick={() => confirmAction({ title: `Remove all mail chaos rules of ${scope}?`, message: 'SMTP sessions stop being disturbed immediately and the counters start over.', confirm: 'Remove all', onConfirm: () => clear.mutate() })}
          >
            Remove all
          </Button>
        )}
      </Group>
      {!rules.length ? (
        <EmptyState title="No mail chaos rules" icon={<IconFlame size={30} />} docs={{ label: 'Mail chaos in the mail guide', href: docsUrl('guide/mail') }} testId="mail-chaos-empty">
          Every SMTP session and provider call succeeds. {canEdit ? 'Add a rule below, for example to make every third message fail with 451.' : ''}
        </EmptyState>
      ) : (
        <DataTable testId="mail-chaos-table" miw={980}>
          <Table.Thead>
            <Table.Tr>
              <Table.Th w={36}>#</Table.Th>
              <Table.Th>Rule</Table.Th>
              <Table.Th>Stage</Table.Th>
              <Table.Th>Match</Table.Th>
              <Table.Th>Effect</Table.Th>
              <Table.Th>Fires</Table.Th>
              <Table.Th>Matched / fired</Table.Th>
              {canEdit && (
                <Table.Th w={150}>
                  <span className="sr-only">Actions</span>
                </Table.Th>
              )}
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {rules.map((r, i) => {
              const c = r.id ? counters.get(r.id) : undefined;
              return (
                <Table.Tr key={`${r.id ?? ''}-${String(i)}`} data-testid="mail-chaos-row" className={editing === i ? 'fm-row-selected' : undefined}>
                  <Table.Td c="dimmed" className="tabular">
                    {i + 1}
                  </Table.Td>
                  <Table.Td>
                    <Text size="sm" fw={700} className="mono fm-nowrap">
                      {r.id || <Text span c="dimmed" size="sm">(new)</Text>}
                    </Text>
                  </Table.Td>
                  <Table.Td data-label="Stage">
                    <span className="fm-chip fm-chip-soft">{r.stage}</span>
                  </Table.Td>
                  <Table.Td data-label="Match">
                    <MatchSummary r={r} />
                  </Table.Td>
                  <Table.Td data-label="Effect">
                    <Group gap={4} wrap="wrap">
                      {actionText(r).map((t) => (
                        <span key={t} className="fm-status" data-tone={t.startsWith('delay') ? 'info' : t.startsWith('4') ? 'warning' : 'danger'}>
                          {t}
                        </span>
                      ))}
                    </Group>
                  </Table.Td>
                  <Table.Td data-label="Fires">
                    <Text size="sm">{firingText(r)}</Text>
                  </Table.Td>
                  <Table.Td data-label="Matched / fired">
                    <span className={classes.counter} data-testid="mail-chaos-counter">
                      {c ? `${String(c.matched)} / ${String(c.fired)}` : dirty ? '—' : '0 / 0'}
                    </span>
                  </Table.Td>
                  {canEdit && (
                    <Table.Td>
                      <Group gap={2} justify="flex-end" wrap="nowrap" className="fm-row-actions">
                        <Tooltip label="Move up">
                          <ActionIcon variant="subtle" color="gray" disabled={i === 0} aria-label={`Move ${r.id ?? 'rule'} up`} onClick={() => change(rules.map((x, j) => (j === i - 1 ? r : j === i ? (rules[i - 1] as MailChaosRule) : x)))}>
                            <IconArrowUp size={16} />
                          </ActionIcon>
                        </Tooltip>
                        <Tooltip label="Move down">
                          <ActionIcon
                            variant="subtle"
                            color="gray"
                            disabled={i === rules.length - 1}
                            aria-label={`Move ${r.id ?? 'rule'} down`}
                            onClick={() => change(rules.map((x, j) => (j === i + 1 ? r : j === i ? (rules[i + 1] as MailChaosRule) : x)))}
                          >
                            <IconArrowDown size={16} />
                          </ActionIcon>
                        </Tooltip>
                        <Tooltip label="Edit">
                          <ActionIcon
                            variant="subtle"
                            color="gray"
                            aria-label={`Edit ${r.id ?? 'rule'}`}
                            onClick={() => {
                              setEditing(i);
                              setDraft(ruleToDraft(r));
                              setTouched(false);
                            }}
                          >
                            <IconPencil size={16} />
                          </ActionIcon>
                        </Tooltip>
                        <Tooltip label="Remove">
                          <ActionIcon
                            variant="subtle"
                            color="red"
                            aria-label={`Remove ${r.id ?? 'rule'}`}
                            onClick={() => {
                              change(rules.filter((_, j) => j !== i));
                              if (editing !== null) resetForm();
                            }}
                          >
                            <IconTrash size={16} />
                          </ActionIcon>
                        </Tooltip>
                      </Group>
                    </Table.Td>
                  )}
                </Table.Tr>
              );
            })}
          </Table.Tbody>
        </DataTable>
      )}
      {canEdit && (
        <SectionCard
          title={editing === null ? 'Add a rule' : `Edit rule ${String(editing + 1)}`}
          description="Checked like the server checks it; nothing changes until you save the rules"
          icon={editing === null ? <IconPlus size={16} /> : <IconPencil size={16} />}
          testId="mail-chaos-editor"
        >
          <RuleForm
            draft={draft}
            setDraft={setDraft}
            issues={issues}
            touched={touched}
            editing={editing !== null}
            onCancel={resetForm}
            onSubmit={() => {
              setTouched(true);
              if (blocked) return;
              change(editing === null ? [...rules, rule] : rules.map((x, j) => (j === editing ? rule : x)));
              resetForm();
            }}
          />
        </SectionCard>
      )}
      {canEdit && (
        <SaveBar dirty={dirty} testId="mail-chaos-save-bar" status={<span className="fm-chip">{`${String(rules.length)} rule${rules.length === 1 ? '' : 's'} · ${scope}`}</span>}>
          {dirty && (
            <Button
              variant="subtle"
              color="gray"
              onClick={() => {
                setDirty(false);
                setRules(q.data?.rules ?? []);
                resetForm();
              }}
            >
              Discard
            </Button>
          )}
          <Button leftSection={<IconDeviceFloppy size={16} />} disabled={!dirty} loading={save.isPending} onClick={() => save.mutate(rules)} data-testid="save-mail-chaos">
            Save rules
          </Button>
        </SaveBar>
      )}
    </Stack>
  );
}
