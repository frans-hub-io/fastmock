import { Group, Text } from '@mantine/core';
import { jsonDiff } from '../lib/diff';

export function DiffView({ left, right, leftLabel = 'before', rightLabel = 'after' }: { left: unknown; right: unknown; leftLabel?: string; rightLabel?: string }) {
  const ops = jsonDiff(left, right);
  const changed = ops.some((o) => o.kind !== 'same');
  return (
    <div>
      <Group gap="md" mb={4}>
        <Text size="xs" className="diff-del" px={6} style={{ borderRadius: 4 }}>
          − {leftLabel}
        </Text>
        <Text size="xs" className="diff-add" px={6} style={{ borderRadius: 4 }}>
          + {rightLabel}
        </Text>
        {!changed && (
          <Text size="xs" c="dimmed">
            identical
          </Text>
        )}
      </Group>
      <pre className="pre" data-testid="diff-view" aria-label={`Differences between ${leftLabel} and ${rightLabel}`}>
        {ops.map((o, i) => (
          <div key={i} className={o.kind === 'add' ? 'diff-add' : o.kind === 'del' ? 'diff-del' : undefined}>
            {o.kind === 'add' ? '+ ' : o.kind === 'del' ? '- ' : '  '}
            {o.text}
          </div>
        ))}
      </pre>
    </div>
  );
}
