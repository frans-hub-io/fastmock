import {
  ActionIcon,
  Button,
  Checkbox,
  Group,
  Select,
  Stack,
  Text,
  TextInput,
} from "@mantine/core";
import { IconPlus, IconTrash } from "@tabler/icons-react";
import { useEffect, useRef, useState } from "react";
import type { StringMatcher } from "../api/types";
import {
  mapToRows,
  MATCHER_OPS,
  OP_LABELS,
  rowsToMap,
  type MatcherRow,
} from "../lib/matchers";

interface Props {
  label: string;
  keyLabel: string;
  value: Record<string, StringMatcher> | undefined;
  onChange: (v: Record<string, StringMatcher> | undefined) => void;
  readOnly?: boolean;
  testId?: string;
}

export function MatcherList({
  label,
  keyLabel,
  value,
  onChange,
  readOnly,
  testId,
}: Props) {
  const [rows, setRows] = useState<MatcherRow[]>(() => mapToRows(value));
  const lastEmitted = useRef(JSON.stringify(value ?? null));

  useEffect(() => {
    const incoming = JSON.stringify(value ?? null);
    if (incoming !== lastEmitted.current) {
      lastEmitted.current = incoming;
      setRows(mapToRows(value));
    }
  }, [value]);

  const update = (next: MatcherRow[]) => {
    setRows(next);
    const m = rowsToMap(next);
    lastEmitted.current = JSON.stringify(m ?? null);
    onChange(m);
  };

  return (
    <Stack gap={6} data-testid={testId}>
      <Group justify="space-between">
        <Text size="sm" fw={500}>
          {label}
        </Text>
        {!readOnly && (
          <Button
            size="compact-xs"
            variant="subtle"
            leftSection={<IconPlus size={14} />}
            onClick={() =>
              update([
                ...rows,
                { key: "", op: "equalTo", value: "", caseInsensitive: false },
              ])
            }
          >
            Add
          </Button>
        )}
      </Group>
      {!rows.length && (
        <Text size="xs" c="dimmed">
          No conditions.
        </Text>
      )}
      {rows.map((r, i) => (
        <Group key={i} gap={6} wrap="nowrap" align="flex-start">
          <TextInput
            size="xs"
            placeholder={keyLabel}
            aria-label={`${keyLabel} ${i + 1}`}
            value={r.key}
            readOnly={readOnly}
            onChange={(e) =>
              update(
                rows.map((x, j) =>
                  j === i ? { ...x, key: e.currentTarget.value } : x,
                ),
              )
            }
            style={{ flex: "1 1 30%" }}
          />
          <Select
            size="xs"
            aria-label={`Operator ${i + 1}`}
            data={(r.op === "complex"
              ? [...MATCHER_OPS, "complex" as const]
              : MATCHER_OPS
            ).map((o) => ({ value: o, label: OP_LABELS[o] }))}
            value={r.op}
            readOnly={readOnly}
            disabled={r.op === "complex"}
            allowDeselect={false}
            onChange={(v) =>
              v && update(rows.map((x, j) => (j === i ? { ...x, op: v } : x)))
            }
            w={150}
          />
          <TextInput
            size="xs"
            placeholder={r.op === "present" || r.op === "absent" ? "" : "value"}
            aria-label={`Value ${i + 1}`}
            value={r.value}
            readOnly={readOnly || r.op === "complex"}
            disabled={r.op === "present" || r.op === "absent"}
            onChange={(e) =>
              update(
                rows.map((x, j) =>
                  j === i ? { ...x, value: e.currentTarget.value } : x,
                ),
              )
            }
            style={{ flex: "1 1 40%" }}
            className={r.op === "complex" ? "mono" : undefined}
          />
          {(r.op === "equalTo" || r.op === "contains") && (
            <Checkbox
              size="xs"
              mt={6}
              label="Aa"
              title="Case-insensitive"
              aria-label={`Case-insensitive ${i + 1}`}
              checked={r.caseInsensitive}
              disabled={readOnly}
              onChange={(e) =>
                update(
                  rows.map((x, j) =>
                    j === i
                      ? { ...x, caseInsensitive: e.currentTarget.checked }
                      : x,
                  ),
                )
              }
            />
          )}
          {!readOnly && (
            <ActionIcon
              variant="subtle"
              color="red"
              aria-label={`Remove ${keyLabel} ${i + 1}`}
              onClick={() => update(rows.filter((_, j) => j !== i))}
            >
              <IconTrash size={14} />
            </ActionIcon>
          )}
        </Group>
      ))}
    </Stack>
  );
}

interface KvProps<V extends string | string[]> {
  label: string;
  value: Record<string, V> | undefined;
  onChange: (v: Record<string, V> | undefined) => void;
  readOnly?: boolean;
  keyPlaceholder?: string;
  /** Rows with the same name become an array of values (response headers such as Set-Cookie). */
  multi?: boolean;
}

function toRows(v: Record<string, string | string[]> | undefined) {
  return Object.entries(v ?? {}).flatMap(([k, val]) =>
    Array.isArray(val) ? val.map((x) => ({ k, v: x })) : [{ k, v: val }],
  );
}

export function KeyValueEditor<V extends string | string[] = string>({
  label,
  value,
  onChange,
  readOnly,
  keyPlaceholder = "name",
  multi,
}: KvProps<V>) {
  const [rows, setRows] = useState(() => toRows(value));
  const lastEmitted = useRef(JSON.stringify(value ?? null));

  useEffect(() => {
    const incoming = JSON.stringify(value ?? null);
    if (incoming !== lastEmitted.current) {
      lastEmitted.current = incoming;
      setRows(toRows(value));
    }
  }, [value]);

  const update = (next: { k: string; v: string }[]) => {
    setRows(next);
    const m: Record<string, string | string[]> = {};
    for (const r of next) {
      const k = r.k.trim();
      if (!k) continue;
      const prev = m[k];
      m[k] =
        multi && prev !== undefined
          ? [...(Array.isArray(prev) ? prev : [prev]), r.v]
          : r.v;
    }
    const out = Object.keys(m).length ? (m as Record<string, V>) : undefined;
    lastEmitted.current = JSON.stringify(out ?? null);
    onChange(out);
  };

  return (
    <Stack gap={6}>
      <Group justify="space-between">
        <Text size="sm" fw={500}>
          {label}
        </Text>
        {!readOnly && (
          <Button
            size="compact-xs"
            variant="subtle"
            leftSection={<IconPlus size={14} />}
            onClick={() => update([...rows, { k: "", v: "" }])}
          >
            Add
          </Button>
        )}
      </Group>
      {!rows.length && (
        <Text size="xs" c="dimmed">
          None.
        </Text>
      )}
      {rows.map((r, i) => (
        <Group key={i} gap={6} wrap="nowrap">
          <TextInput
            size="xs"
            placeholder={keyPlaceholder}
            aria-label={`${label} name ${i + 1}`}
            value={r.k}
            readOnly={readOnly}
            onChange={(e) =>
              update(
                rows.map((x, j) =>
                  j === i ? { ...x, k: e.currentTarget.value } : x,
                ),
              )
            }
            style={{ flex: "1 1 35%" }}
          />
          <TextInput
            size="xs"
            placeholder="value"
            aria-label={`${label} value ${i + 1}`}
            value={r.v}
            readOnly={readOnly}
            onChange={(e) =>
              update(
                rows.map((x, j) =>
                  j === i ? { ...x, v: e.currentTarget.value } : x,
                ),
              )
            }
            style={{ flex: "1 1 65%" }}
          />
          {!readOnly && (
            <ActionIcon
              variant="subtle"
              color="red"
              aria-label={`Remove ${label} ${i + 1}`}
              onClick={() => update(rows.filter((_, j) => j !== i))}
            >
              <IconTrash size={14} />
            </ActionIcon>
          )}
        </Group>
      ))}
    </Stack>
  );
}
