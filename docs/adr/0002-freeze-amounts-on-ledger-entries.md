# Freeze Cost and Price on each Ledger Entry

Each Ledger Entry stores its computed Cost and Price (integer micro-dollars) and the Card Versions that produced them. A change to a Cost Card or Price Card does not reprice old Ledger Entries. We chose this over computing amounts at query time from token counts, because invoices and Profit reports for past periods must not change when the Operator edits a card. Unpriced Entries are the one exception: the Operator can price them after a card is added.
