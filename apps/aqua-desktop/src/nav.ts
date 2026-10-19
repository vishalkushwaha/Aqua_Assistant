import {
  Volume2, Workflow,
  GitBranch, LayoutDashboard, Lock, FolderSearch, Headphones, Move, ShieldAlert, Brain, Network, Building2,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import { KeyRound as PermissionsIcon } from 'lucide-react';
import { BookOpen as MemoryIcon, Inbox as SkillsInboxIcon, Library as VaultIcon } from 'lucide-react';

export type TabId =
  | 'overview'
  | 'tts-voices'
  | 'security'
  | 'workflows'
  | 'flows'
  | 'tools'
  | 'permissions'
  | 'memory'
  | 'graph-memory'
  | 'company-brain'
  | 'agent-learning'
  | 'skills-inbox'
  | 'file-index'
  | 'vault'
  | 'devices'
  | 'orb-position'
  | 'showcase-capabilities'
  | 'showcase-local-ai'
  | 'showcase-voice'
  | 'showcase-automation'
  | 'showcase-knowledge'
  | 'showcase-platform';

export interface NavItem {
  id: TabId;
  label: string;
  icon: LucideIcon;
}

/** The clean navigation menu, organized by user function with professional terminology. */
export const NAV_GROUPS: { label: string; items: NavItem[] }[] = [
  {
    label: 'Assistant',
    items: [
      { id: 'overview', label: 'Overview', icon: LayoutDashboard },
      { id: 'tts-voices', label: 'Voice Profiles', icon: Volume2 },
      { id: 'security', label: 'Voice Security', icon: Lock },
    ],
  },
  {
    label: 'Automations',
    items: [
      { id: 'workflows', label: 'Workflows', icon: Workflow },
      { id: 'flows', label: 'Automated Routines', icon: GitBranch },
      { id: 'tools', label: 'App Approvals', icon: ShieldAlert },
      { id: 'permissions', label: 'Permissions & Privacy', icon: PermissionsIcon },
    ],
  },
  {
    label: 'Knowledge & Vault',
    items: [
      { id: 'memory', label: 'Memory', icon: MemoryIcon },
      { id: 'graph-memory', label: 'Memory Graph (FalkorDB)', icon: Network },
      { id: 'company-brain', label: 'Company Brain', icon: Building2 },
      { id: 'agent-learning', label: 'Preferences & Learning', icon: Brain },
      { id: 'skills-inbox', label: 'Extensions & Skills', icon: SkillsInboxIcon },
      { id: 'file-index', label: 'File Search Index', icon: FolderSearch },
      { id: 'vault', label: 'Secure Vault', icon: VaultIcon },
    ],
  },
  {
    label: 'Device Settings',
    items: [
      { id: 'devices', label: 'Audio Devices', icon: Headphones },
      { id: 'orb-position', label: 'Display Settings', icon: Move },
    ],
  },
];

export const ALL_TABS: TabId[] = NAV_GROUPS.flatMap((g) => g.items.map((i) => i.id));
