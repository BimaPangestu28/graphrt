- Plugins: a manifest file that uses a key, runtime or value this version does
  not know (for example a plugin made for a newer graphrt) is refused with
  "this plugin uses `triggers`, which this version of graphrt does not know ...
  update graphrt and try again", followed by the parser's own message, instead
  of only "unknown field `triggers`, expected one of ...". `graphrt plugin
  install` and every host that loads plugin folders show it.
