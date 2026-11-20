-- Minimal stand-in for the parts of Supabase the migration and isolation test
-- use (roles anon/authenticated, auth.users, auth.uid(), default privileges), so
-- both run on plain Postgres in CI without the Supabase CLI. Never apply this to
-- a real Supabase project: it already has all of this.
--
--   createdb chatbot_test
--   psql -d chatbot_test -v ON_ERROR_STOP=1 -f supabase/tests/plain-postgres-shim.sql \
--     -f supabase/migrations/20261120000000_chatbot.sql -f supabase/tests/isolation.sql
create role anon nologin; create role authenticated nologin;
grant anon, authenticated to current_user;
create schema auth;
create table auth.users (id uuid primary key, email text);
create function auth.uid() returns uuid language sql stable as $$
  select coalesce(nullif(current_setting('request.jwt.claim.sub', true), ''),
                  (nullif(current_setting('request.jwt.claims', true), '')::jsonb ->> 'sub'))::uuid $$;
grant usage on schema auth, public to anon, authenticated;
grant execute on function auth.uid() to anon, authenticated;
-- Supabase's default privileges: every role gets new public tables and functions.
alter default privileges in schema public grant all on tables to anon, authenticated;
alter default privileges in schema public grant all on functions to anon, authenticated;
