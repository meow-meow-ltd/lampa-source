import Arrays from '../../utils/arrays'
import Storage from './storage'
import ProfileSettings from '../../services/profile_settings'
import AccountListener from '../account/listener'

const PROFILE_KEYS = {
    torrents_view: { empty: [], type: 'array' },
    torrents_filter_data: { empty: {}, type: 'object' },
    torrents_filter: { empty: {}, type: 'object' }
}

const migration_key = 'torrents_profile_migrated'
let loading = false
let load_promise = Promise.resolve()

function canUseProfileStorage(){
    return (window.lampa_settings.account_provider || 'cub') === 'local' && window.lampa_settings.account_use
}

function currentProfileId(){
    if(window.Lampa && Lampa.Account && Lampa.Account.Permit){
        return Lampa.Account.Permit.profileId
    }

    let account = Storage.get('account', '{}') || {}

    return account.profile && account.profile.id ? account.profile.id : 0
}

function parseJson(raw){
    try{
        return JSON.parse(raw)
    }
    catch(e){
        return null
    }
}

function emptyValue(key){
    return Arrays.clone(PROFILE_KEYS[key].empty)
}

function normalizeValue(key, value){
    if(value === null || typeof value === 'undefined') return null

    if(typeof value === 'string'){
        if(!value) return emptyValue(key)

        let parsed = parseJson(value)

        value = parsed === null ? value : parsed
    }

    let type = PROFILE_KEYS[key].type

    if(type === 'array' && !Array.isArray(value)) return emptyValue(key)
    if(type === 'object' && !Arrays.isObject(value)) return emptyValue(key)

    return value
}

function ensureProfile(){
    if(window.Lampa && Lampa.Account && Lampa.Account.Profile && Lampa.Account.Permit){
        if(Lampa.Account.Permit.access && !Lampa.Account.Permit.profileId){
            return new Promise((resolve)=>{
                Lampa.Account.Profile.check(resolve)
            })
        }
    }

    return Promise.resolve()
}

function load(){
    if(!canUseProfileStorage()) return Promise.resolve()

    loading = true

    load_promise = ensureProfile().then(()=>{
        let profile_id = currentProfileId()

        if(!profile_id) return

        let keys = Object.keys(PROFILE_KEYS)

        return Promise.all(keys.map((key)=>ProfileSettings.get(key))).then((values)=>{
            keys.forEach((key, index)=>{
                let value = normalizeValue(key, values[index])

                if(value === null){
                    Storage.set(key, emptyValue(key), true)
                    return
                }

                Storage.set(key, value, true)
            })
        })
    }).finally(()=>{
        loading = false
    })

    return load_promise
}

function onStorageChange(e){
    if(!PROFILE_KEYS[e.name]) return
    if(!canUseProfileStorage()) return
    if(loading) return

    ProfileSettings.set(e.name, e.value)
}

function init(){
    if(!canUseProfileStorage()) return

    Storage.listener.follow('change', onStorageChange)
    AccountListener.follow('profile_select', load)

    load()
}

export default {
    init,
    load
}
