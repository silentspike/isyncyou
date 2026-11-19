package com.silentspike.isyncyou

/** A pass is complete only when every account observed at its start succeeded. */
internal object ProviderCatchUpPolicy {
    fun complete(accounts: List<String>, outcomes: Map<String, Boolean>): Boolean =
        accounts.all { outcomes[it] == true }
}
