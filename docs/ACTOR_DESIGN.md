Clean way to handle common mailbox patterns on actor:
- send type with oneshot, wait response
- send type, no response

Usually has 1 big enum to send

Minimal design:
- Mailbox (recv) and MailboxHandle (send)
- client struct generated wraps the MailboxHandle, method for each, automates each part
- server can await mailbox recv, gets a enum from client, enums have oneshots to reply
- Define an enum with macro annotation.
