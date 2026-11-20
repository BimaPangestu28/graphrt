-- Sample schema for the graphrt Supabase chatbot example (examples/supabase-chatbot).
--
-- Security model, all enforced by Postgres, not by the model or graphrt:
--   * Every table has row level security keyed on auth.uid(); a user sees and
--     changes only their own rows. anon has no access at all.
--   * The bot reaches data only through the narrow functions below. They are
--     SECURITY INVOKER (RLS stays in force), take typed arguments, cap their
--     rows and are executable by `authenticated` only. They are exactly the
--     names in SUPABASE_TOOL_RPC_ALLOWLIST.
--   * There is no exec_sql / dynamic SQL function: a prompt-injected model has
--     nothing to call beyond these six.
-- Apply with `supabase db push` (or `supabase db reset` locally). Check with
-- tests/isolation.sql.

-- ---------------------------------------------------------------- tables

create table public.profiles (
  id uuid primary key references auth.users (id) on delete cascade,
  display_name text not null default '' check (length(display_name) <= 80),
  timezone text not null default 'UTC' check (length(timezone) <= 64),
  created_at timestamptz not null default now()
);

create table public.diary_entries (
  id bigint generated always as identity primary key,
  user_id uuid not null default auth.uid() references auth.users (id) on delete cascade,
  entry_date date not null default current_date,
  title text not null default '' check (length(title) <= 200),
  body text not null default '' check (length(body) <= 20000),
  created_at timestamptz not null default now()
);
create index diary_entries_user_date on public.diary_entries (user_id, entry_date desc);

create table public.important_dates (
  id bigint generated always as identity primary key,
  user_id uuid not null default auth.uid() references auth.users (id) on delete cascade,
  label text not null check (length(label) between 1 and 200),
  occurs_on date not null,
  repeats_yearly boolean not null default true
);
create index important_dates_user on public.important_dates (user_id, occurs_on);

create table public.chat_sessions (
  id uuid primary key default gen_random_uuid(),
  user_id uuid not null default auth.uid() references auth.users (id) on delete cascade,
  title text not null default 'New chat' check (length(title) <= 120),
  created_at timestamptz not null default now()
);
create index chat_sessions_user on public.chat_sessions (user_id, created_at desc);

create table public.chat_messages (
  id bigint generated always as identity primary key,
  session_id uuid not null references public.chat_sessions (id) on delete cascade,
  user_id uuid not null default auth.uid() references auth.users (id) on delete cascade,
  role text not null check (role in ('user', 'assistant')),
  content text not null check (length(content) <= 8000),
  created_at timestamptz not null default now()
);
create index chat_messages_session on public.chat_messages (session_id, created_at);

-- ---------------------------------------------------------------- row level security

alter table public.profiles enable row level security;
alter table public.diary_entries enable row level security;
alter table public.important_dates enable row level security;
alter table public.chat_sessions enable row level security;
alter table public.chat_messages enable row level security;

-- `(select auth.uid())` is evaluated once per statement instead of once per row.
create policy "own profile" on public.profiles
  for all to authenticated
  using (id = (select auth.uid())) with check (id = (select auth.uid()));

create policy "own diary entries" on public.diary_entries
  for all to authenticated
  using (user_id = (select auth.uid())) with check (user_id = (select auth.uid()));

create policy "own important dates" on public.important_dates
  for all to authenticated
  using (user_id = (select auth.uid())) with check (user_id = (select auth.uid()));

create policy "own chat sessions" on public.chat_sessions
  for all to authenticated
  using (user_id = (select auth.uid())) with check (user_id = (select auth.uid()));

-- A message must belong to the user *and* go into one of the user's sessions.
create policy "own chat messages" on public.chat_messages
  for all to authenticated
  using (user_id = (select auth.uid()))
  with check (
    user_id = (select auth.uid())
    and exists (select 1 from public.chat_sessions s where s.id = session_id and s.user_id = (select auth.uid()))
  );

-- Supabase grants every role access to new public tables by default; anon gets none here.
revoke all on public.profiles, public.diary_entries, public.important_dates,
  public.chat_sessions, public.chat_messages from anon;
grant select, insert, update, delete on public.profiles, public.diary_entries, public.important_dates,
  public.chat_sessions, public.chat_messages to authenticated;

-- ---------------------------------------------------------------- bot functions (the RPC allowlist)

create function public.get_my_profile()
returns setof public.profiles
language sql stable security invoker set search_path = ''
as $$
  select * from public.profiles where id = (select auth.uid()) limit 1;
$$;

create function public.list_diary_entries(from_date date default null, to_date date default null)
returns setof public.diary_entries
language sql stable security invoker set search_path = ''
as $$
  select * from public.diary_entries
  where user_id = (select auth.uid())
    and (list_diary_entries.from_date is null or entry_date >= list_diary_entries.from_date)
    and (list_diary_entries.to_date is null or entry_date <= list_diary_entries.to_date)
  order by entry_date desc, id desc
  limit 50;
$$;

create function public.list_important_dates()
returns setof public.important_dates
language sql stable security invoker set search_path = ''
as $$
  select * from public.important_dates
  where user_id = (select auth.uid())
  order by occurs_on
  limit 100;
$$;

create function public.list_chat_messages(session_id uuid)
returns setof public.chat_messages
language sql stable security invoker set search_path = ''
as $$
  select * from (
    select * from public.chat_messages m
    where m.session_id = list_chat_messages.session_id and m.user_id = (select auth.uid())
    order by m.created_at desc, m.id desc
    limit 50
  ) latest
  order by created_at, id;
$$;

create function public.start_chat_session(title text default null)
returns public.chat_sessions
language sql volatile security invoker set search_path = ''
as $$
  insert into public.chat_sessions (title)
  values (left(coalesce(nullif(trim(start_chat_session.title), ''), 'New chat'), 120))
  returning *;
$$;

create function public.add_chat_message(session_id uuid, role text, content text)
returns public.chat_messages
language sql volatile security invoker set search_path = ''
as $$
  insert into public.chat_messages (session_id, role, content)
  values (add_chat_message.session_id, add_chat_message.role, left(add_chat_message.content, 8000))
  returning *;
$$;

-- Functions are executable by PUBLIC (and Supabase's default privileges add anon):
-- only signed-in users may call these.
revoke execute on function
  public.get_my_profile(), public.list_diary_entries(date, date), public.list_important_dates(),
  public.list_chat_messages(uuid), public.start_chat_session(text), public.add_chat_message(uuid, text, text)
  from public, anon;
grant execute on function
  public.get_my_profile(), public.list_diary_entries(date, date), public.list_important_dates(),
  public.list_chat_messages(uuid), public.start_chat_session(text), public.add_chat_message(uuid, text, text)
  to authenticated;
