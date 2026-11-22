import { Stack, Tabs } from '@mantine/core';
import { IconInbox, IconPlugConnected } from '@tabler/icons-react';
import { useNavigate, useParams } from 'react-router';
import { useCaptureFeature } from '../api/capture';
import { PageHeader } from '../components/common';
import { LoadingState } from '../components/States';
import { MessageList } from './messages/MessageList';
import { MessageView } from './messages/MessageView';
import { CaptureOff } from './messages/parts';
import { Sources } from './messages/Sources';

export type MessagesSection = 'messages' | 'sources' | 'message';

const SECTIONS: { value: Exclude<MessagesSection, 'message'>; label: string; to: string; icon: typeof IconInbox }[] = [
  { value: 'messages', label: 'Messages', to: '/messages', icon: IconInbox },
  { value: 'sources', label: 'Sources', to: '/messages/sources', icon: IconPlugConnected },
];

const DESCRIPTIONS: Record<Exclude<MessagesSection, 'message'>, string> = {
  messages: 'Messages captured from Kafka, MQTT and AMQP brokers, newest first.',
  sources: 'The topics, topic filters and exchanges FastMock subscribes to, and how their consumers are doing.',
};

/** The Messages screen: captured broker messages, one message and the capture sources. Shown only when the server captures messages. */
export default function Messages({ section = 'messages' }: { section?: MessagesSection }) {
  const feature = useCaptureFeature();
  const navigate = useNavigate();
  const { id } = useParams();
  if (feature === 'loading') return <LoadingState />;
  if (feature === 'off')
    return (
      <Stack gap="md">
        <PageHeader title="Messages" description={DESCRIPTIONS.messages} />
        <CaptureOff />
      </Stack>
    );
  if (section === 'message' && id) return <MessageView key={id} id={id} />;
  const current = section === 'message' ? 'messages' : section;
  return (
    <Stack gap="md">
      <PageHeader title="Messages" description={DESCRIPTIONS[current]} />
      <Tabs
        value={current}
        onChange={(v) => {
          const s = SECTIONS.find((x) => x.value === v);
          if (s) void navigate(s.to);
        }}
        variant="pills"
        radius="md"
        classNames={{ list: 'fm-tabs-list' }}
        keepMounted={false}
      >
        <Tabs.List>
          {SECTIONS.map((s) => (
            <Tabs.Tab key={s.value} value={s.value} leftSection={<s.icon size={16} />} data-testid={`messages-section-${s.value}`}>
              {s.label}
            </Tabs.Tab>
          ))}
        </Tabs.List>
        <Tabs.Panel value="messages" pt="md">
          <MessageList />
        </Tabs.Panel>
        <Tabs.Panel value="sources" pt="md">
          <Sources />
        </Tabs.Panel>
      </Tabs>
    </Stack>
  );
}
