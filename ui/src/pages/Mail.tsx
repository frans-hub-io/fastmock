import { Stack, Tabs } from '@mantine/core';
import { IconFlame, IconMail, IconMailbox } from '@tabler/icons-react';
import { useNavigate, useParams } from 'react-router';
import { useMailFeature } from '../api/mail';
import { PageHeader } from '../components/common';
import { LoadingState } from '../components/States';
import { MailChaos } from './mail/MailChaos';
import { MailList } from './mail/MailList';
import { MailMailboxes } from './mail/MailMailboxes';
import { MessageView } from './mail/MailMessage';
import { MailOff } from './mail/parts';

export type MailSection = 'messages' | 'mailboxes' | 'chaos' | 'message';

const SECTIONS: { value: Exclude<MailSection, 'message'>; label: string; to: string; icon: typeof IconMail }[] = [
  { value: 'messages', label: 'Messages', to: '/mail', icon: IconMail },
  { value: 'mailboxes', label: 'Mailboxes', to: '/mail/mailboxes', icon: IconMailbox },
  { value: 'chaos', label: 'Mail chaos', to: '/mail/chaos', icon: IconFlame },
];

const DESCRIPTIONS: Record<Exclude<MailSection, 'message'>, string> = {
  messages: 'Email your applications sent over SMTP and provider APIs, newest first. Nothing is ever delivered.',
  mailboxes: 'IMAP and POP3 mailboxes of the recipients, and their optional passwords.',
  chaos: 'Make SMTP sessions and provider API calls fail on purpose to test retries and error handling.',
};

/** The Mail screen: messages, one message, mailboxes and mail chaos rules. Shown only when the server runs the mail module. */
export default function Mail({ section = 'messages' }: { section?: MailSection }) {
  const feature = useMailFeature();
  const navigate = useNavigate();
  const { id } = useParams();
  if (feature.state === 'loading') return <LoadingState />;
  if (feature.state === 'off')
    return (
      <Stack gap="md">
        <PageHeader title="Mail" description={DESCRIPTIONS.messages} />
        <MailOff />
      </Stack>
    );
  if (section === 'message' && id) return <MessageView key={id} id={id} />;
  const current = section === 'message' ? 'messages' : section;
  return (
    <Stack gap="md">
      <PageHeader title="Mail" description={DESCRIPTIONS[current]} />
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
            <Tabs.Tab key={s.value} value={s.value} leftSection={<s.icon size={16} />} data-testid={`mail-section-${s.value}`}>
              {s.label}
            </Tabs.Tab>
          ))}
        </Tabs.List>
        <Tabs.Panel value="messages" pt="md">
          <MailList />
        </Tabs.Panel>
        <Tabs.Panel value="mailboxes" pt="md">
          <MailMailboxes />
        </Tabs.Panel>
        <Tabs.Panel value="chaos" pt="md">
          <MailChaos />
        </Tabs.Panel>
      </Tabs>
    </Stack>
  );
}
