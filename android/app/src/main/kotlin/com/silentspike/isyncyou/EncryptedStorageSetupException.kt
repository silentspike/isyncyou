package com.silentspike.isyncyou

/** Startup-blocking encrypted-storage setup failure. Local data must not be opened after this. */
class EncryptedStorageSetupException(message: String, cause: Throwable? = null) :
    Exception(message, cause)
