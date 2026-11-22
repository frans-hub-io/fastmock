import { IconPlugConnectedX } from '@tabler/icons-react';
import { CommandSnippet } from '../../components/common';
import { EmptyState } from '../../components/States';
import { protocolLabel } from '../../lib/capture';

const KIND: Record<string, string> = { kafka: 'event', mqtt: 'grpc', amqp: 'soap' };

/** The broker of a message or source as a coloured pill. */
export function ProtocolBadge({ protocol }: { protocol: string | null | undefined }) {
  const p = protocol ?? 'kafka';
  return (
    <span className="fm-kind" data-kind={KIND[p] ?? 'generic'} data-testid="protocol-badge">
      {protocolLabel(p)}
    </span>
  );
}

/** What the Messages page shows when the server runs without message capture. */
export function CaptureOff() {
  return (
    <div data-testid="capture-off">
      <EmptyState title="Message capture is not enabled on this server" icon={<IconPlugConnectedX size={26} />}>
        FastMock stores the messages your applications publish to Kafka, MQTT and AMQP brokers when message capture is on. Start the server with:
        <div style={{ marginTop: 10 }}>
          <CommandSnippet command="FASTMOCK_CAPTURE_ENABLED=true" label="Copy setting" />
        </div>
      </EmptyState>
    </div>
  );
}
