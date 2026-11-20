import { Badge, Select, Text } from '@mantine/core';
import { IconStack2 } from '@tabler/icons-react';
import type { Permission } from '../auth/permissions';
import { useSpace, useSpaceOptions } from '../auth/useAuth';

const ALL = '*';
const GLOBAL = '~global';

/** The header's space selector; nothing when the server has no spaces. */
export function SpaceSelect() {
  const { enabled, spaces, current, allowAll, setCurrent } = useSpace();
  if (!enabled) return null;
  const data = [...(allowAll ? [{ value: ALL, label: 'All spaces' }] : []), ...spaces.map((s) => ({ value: s.name, label: s.name }))];
  return (
    <Select
      data={data}
      value={current ?? ALL}
      onChange={(v) => setCurrent(v === null || v === ALL ? null : v)}
      allowDeselect={false}
      aria-label="Space"
      leftSection={<IconStack2 size={16} aria-hidden />}
      size="sm"
      w="100%"
      comboboxProps={{ width: 220, position: 'bottom-start' }}
      data-testid="space-select"
      renderOption={({ option }) => {
        const def = spaces.find((s) => s.name === option.value);
        return (
          <div>
            <Text size="sm">{option.label}</Text>
            {def?.description && (
              <Text size="xs" c="dimmed" lineClamp={2}>
                {def.description}
              </Text>
            )}
          </div>
        );
      }}
    />
  );
}

/** A space shown in a table column or detail view; the global area is a dash. */
export function SpaceBadge({ space }: { space: string | null | undefined }) {
  if (!space)
    return (
      <Text span size="sm" c="dimmed" aria-label="global area">
        —
      </Text>
    );
  return (
    <Badge variant="light" color="teal" radius="sm" tt="none" data-testid="space-badge">
      {space}
    </Badge>
  );
}

/**
 * Where a create or import form writes: the global area (if `perm` is held globally) or a space where it is
 * held. Pair it with `useSpaceChoice`; hidden when the server has no spaces.
 */
export function SpaceField({
  perm,
  value,
  onChange,
  label = 'Space',
  description = 'Where the new items go',
}: {
  perm: Permission;
  value: string | null;
  onChange: (v: string | null) => void;
  label?: string;
  description?: string;
}) {
  const { enabled } = useSpace();
  const options = useSpaceOptions(perm);
  if (!enabled) return null;
  return (
    <Select
      label={label}
      description={description}
      data={options.map((o) => ({ value: o ?? GLOBAL, label: o ?? 'Global area (no space)' }))}
      value={value ?? GLOBAL}
      onChange={(v) => onChange(v && v !== GLOBAL ? v : null)}
      allowDeselect={false}
      data-testid="space-field"
      w={{ base: '100%', sm: 280 }}
    />
  );
}
