import { Button } from '@mantine/core';
import { Link } from 'react-router';
import { EmptyState } from '../components/States';

export default function NotFound() {
  return (
    <EmptyState title="Page not found">
      <Button component={Link} to="/" variant="light" mt="sm">
        Back to the dashboard
      </Button>
    </EmptyState>
  );
}
