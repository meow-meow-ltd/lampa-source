import Api from '../core/account/api'
import Permit from '../core/account/permit'

function canUse() {
    return Permit.access && Permit.profileId
}

function get(key) {
    return new Promise((resolve) => {
        if (!canUse()) return resolve(null)

        Api.load('profile/settings?key=' + encodeURIComponent(key))
            .then((result) => {
                if (result && Object.prototype.hasOwnProperty.call(result, 'value')) resolve(result.value)
                else resolve(null)
            })
            .catch(() => resolve(null))
    })
}

function set(key, value) {
    if (value === null) return;
    return new Promise((resolve) => {
        if (!canUse()) return resolve(false)

        Api.load('profile/settings', {}, { key, value })
            .then(() => resolve(true))
            .catch(() => resolve(false))
    })
}

export default {
    get,
    set
}
