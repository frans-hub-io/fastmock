import { Tooltip } from '@mantine/core';
import { IconPlugConnectedX } from '@tabler/icons-react';
import type { MailEntry } from '../../api/mail';
import { Copy, CommandSnippet } from '../../components/common';
import { EmptyState } from '../../components/States';
import { docsUrl } from '../../lib/docs';
import { sourceLabel } from '../../lib/mail';
import type { StreamStatus } from '../../lib/mailStream';
import classes from './Mail.module.css';

/** How a message arrived: SMTP, a provider API (named), or injected through the admin API. */
export function SourceBadge({ entry }: { entry: Pick<MailEntry, 'source' | 'provider'> }) {
  const title = entry.source === 'provider' ? `Sent through the ${sourceLabel(entry)} API` : entry.source === 'smtp' ? 'Received over SMTP' : 'Injected through the admin API';
  return (
    <Tooltip label={title} withArrow openDelay={300}>
      <span className={classes.source} data-source={entry.source} data-testid="mail-source">
        {sourceLabel(entry)}
      </span>
    </Tooltip>
  );
}

export function LabelChip({ label }: { label: string }) {
  return (
    <span className={classes.label} data-testid="mail-label">
      {label}
    </span>
  );
}

/** A multi-line snippet on a dark code surface with a copy button. */
export function Snippet({ code, label = 'Copy snippet', testId }: { code: string; label?: string; testId?: string }) {
  return (
    <div className={classes.snippetBox}>
      <pre className={classes.snippet} data-testid={testId}>
        {code}
      </pre>
      <div className={classes.snippetCopy}>
        <Copy value={code} label={label} />
      </div>
    </div>
  );
}

const STATUS_TEXT: Record<StreamStatus, string> = {
  connecting: 'Connecting to live updates…',
  live: 'Live',
  reconnecting: 'Reconnecting…',
  off: 'Live updates off',
};

export function LiveStatus({ status }: { status: StreamStatus }) {
  return (
    <span className="fm-live" data-live={status === 'live' || undefined} data-testid="mail-live" data-status={status}>
      <span className="fm-live-dot" aria-hidden />
      {STATUS_TEXT[status]}
    </span>
  );
}

/** What the Mail page shows when the server runs without the mail module. */
export function MailOff() {
  return (
    <div data-testid="mail-off">
      <EmptyState title="Mail is not enabled on this server" icon={<IconPlugConnectedX size={26} />} docs={{ label: 'Capturing mail with FastMock', href: docsUrl('guide/mail') }}>
        FastMock captures email over SMTP and provider APIs when the mail module is on. Start the server with:
        <div style={{ marginTop: 10 }}>
          <CommandSnippet command="FASTMOCK_MAIL_ENABLED=true" label="Copy setting" />
        </div>
      </EmptyState>
    </div>
  );
}
